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
//! 1. **A small set the panes cannot take**: `Ctrl+Q` quits and `Ctrl+W` closes the focused pane.
//!    These run *before* the pane for one reason: a pane that swallows everything — a terminal
//!    running a full-screen program, an editor in a modal state — must not be able to trap the
//!    user. Two keys, no more: every key in this set is a key no pane can ever use.
//! 2. **The focused pane's turn**, which answers [`KeyOutcome::Handled`] or [`KeyOutcome::Ignored`].
//! 3. **The rest of the app's keys** — only for keys the pane ignored: `Ctrl+T` cycles focus,
//!    `Alt+1`…`Alt+9` jump to a pane by position.
//!
//! That middle step is why keys are not a match at the top of the loop: `Tab` means "indent" in an
//! editor and "next item" in a file tree, and both must keep it, while `Tab` in a pane with no
//! opinion moves focus. The app does not have to know which pane it is looking at.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jmds_core::{event::AgentEvent, pane::Axis};
use ratatui::{buffer::Buffer, layout::Rect};

use crate::pane::{Pane, PaneHost};

/// What the loop should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Continue,
    Quit,
}

/// The panes, and the keys that belong to the app rather than to them.
pub struct App {
    host: PaneHost,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            host: PaneHost::new(),
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
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if let Some(action) = self.unstealable(key) {
            return action;
        }

        // The focused pane first: `Tab` in an editor is an indent, and only a pane with no use for
        // a key should lose it to the app.
        if self.host.on_key(key).is_handled() {
            return Action::Continue;
        }

        self.app_key(key);
        Action::Continue
    }

    /// The two keys no pane may take, and the only place a pane can be closed from.
    fn unstealable(&mut self, key: KeyEvent) -> Option<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
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

    /// Keys the app answers only when the focused pane had no use for them.
    fn app_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('t') if control => {
                self.host.cycle_focus();
            }
            KeyCode::Char(digit @ '1'..='9') if alt => {
                // Layout order from the tree, not from the last frame's geometry: a key that only
                // works after a redraw is a key that does nothing on the first keystroke, and the
                // geometry is the mouse's business.
                let wanted = digit as usize - '1' as usize;
                if let Some(id) = self.host.tree().leaves().get(wanted) {
                    self.host.focus(*id);
                }
            }
            _ => {}
        }
    }

    /// An event from the engine, for whoever is listening. Nothing here quits: a model that fails,
    /// a tool that errors, a file that changed — none of those are reasons to close the app.
    pub fn on_agent_event(&mut self, event: &AgentEvent) -> Action {
        self.host.on_agent_event(event);
        Action::Continue
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

    /// One frame.
    pub fn draw(&mut self, area: Rect, buf: &mut Buffer) {
        self.host.draw(area, buf);
    }
}

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

    /// A pane whose answers the test chooses, and which records the keys it was given.
    struct Recorder {
        handles: bool,
        keys: Recorded,
        events: Rc<RefCell<usize>>,
    }

    impl Recorder {
        fn new(handles: bool) -> (Self, Recorded) {
            let keys = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    handles,
                    keys: keys.clone(),
                    events: Rc::new(RefCell::new(0)),
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
    }

    fn control(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
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
    fn a_key_the_pane_ignores_is_still_the_apps() {
        // The middle step of the policy, both ways round: the same key, two panes, two outcomes.
        let mut app = App::new();
        let (handling, handled_keys) = Recorder::new(true);
        let first = app.open(Axis::Horizontal, handling);
        let (ignoring, ignored_keys) = Recorder::new(false);
        let second = app.open(Axis::Horizontal, ignoring);

        // The focused pane is asked first, and when it ignores the key the app's own handling runs:
        // which for `Ctrl+T` means focus moves on.
        assert_eq!(app.host().focused_id(), Some(second));
        app.on_key(control(KeyCode::Char('t')));
        assert_eq!(handled_keys.borrow().len(), 0, "that pane handled nothing");
        assert_eq!(
            ignored_keys.borrow().len(),
            1,
            "the focused pane was asked first"
        );
        assert_eq!(
            app.host().focused_id(),
            Some(first),
            "it ignored the key, so the app cycled focus"
        );

        // A pane that takes the key never lets it reach the app's own handling.
        app.host_mut().focus(first);
        handled_keys.borrow_mut().clear();
        app.on_key(control(KeyCode::Char('t')));
        assert_eq!(handled_keys.borrow().as_slice(), &[KeyCode::Char('t')]);
        assert_eq!(
            app.host().focused_id(),
            Some(first),
            "the pane took it, so focus did not move"
        );
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
