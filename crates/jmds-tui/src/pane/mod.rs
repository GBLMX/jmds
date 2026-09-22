//! What a pane *is*, and who decides where it goes.
//!
//! Two halves, deliberately apart:
//!
//! - [`Pane`] is what one pane does — its kind, its title, how it draws its **contents**, what it
//!   does with a key and with an event from the engine. It never learns where it sits, never draws
//!   its own border, and never talks to another pane.
//! - [`PaneHost`] is where they live: it owns the [`PaneTree`] from `jmds-core`, opens and closes
//!   panes, computes each one's rectangle once per frame, draws the borders and titles, sends keys
//!   to the focused pane, and fans engine events out to whoever wants them.
//!
//! The split is what makes the borders, the focus highlight and hit-testing consistent: they are
//! one implementation in the host instead of four in the panes, and they cannot disagree with the
//! geometry, because the geometry the host drew with is the same list [`PaneHost::pane_at`] answers
//! a mouse from.
//!
//! A pane that grows its own opinion about layout is the failure this module exists to prevent.
//!
//! The kinds live beside this file: [`chat`] is the conversation.

pub mod chat;
pub mod editor;
pub mod terminal;

use std::collections::BTreeMap;

use crossterm::event::KeyEvent;
use jmds_core::{
    event::AgentEvent,
    pane::{Axis, PaneId, PaneKind, PaneTree, Rect as PaneRect},
};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Widget},
};

use crate::{effects, theme::Theme};

/// The engine's rectangle for a terminal area.
///
/// The tree keeps its own `Rect` because `jmds-core` does not depend on a terminal library, and
/// this crate keeps drawing in `ratatui`'s because that is what `Buffer` takes. One conversion
/// here, at the only place the two meet, rather than a second layout model that can disagree.
pub const fn to_pane_rect(rect: Rect) -> PaneRect {
    PaneRect::new(rect.x, rect.y, rect.width, rect.height)
}

/// ...and back.
pub const fn from_pane_rect(rect: PaneRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}

/// What a pane did with a key.
///
/// Not a `bool`, because the two answers mean different things to the caller: `Handled` means the
/// key is spent, `Ignored` means the app should now try its own keys — quitting, moving focus. A
/// pane that silently swallowed everything would make `Ctrl+Q` work in three panes out of four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    Handled,
    Ignored,
}

impl KeyOutcome {
    pub const fn is_handled(self) -> bool {
        matches!(self, Self::Handled)
    }
}

/// One pane: a chat transcript, an editor buffer, a terminal, the file tree.
///
/// Implementations are given the area *inside* their border, so a pane draws text and lays out its
/// own viewport; the border, the title and the focus highlight are the host's, which is why focus
/// looks the same everywhere.
pub trait Pane {
    fn kind(&self) -> PaneKind;

    /// What the border shows. Borrowed rather than owned because this is asked once per frame, and
    /// a per-frame allocation per pane is an allocation for nothing.
    fn title(&self) -> &str;

    /// Draw the contents into `area`, which excludes the border.
    ///
    /// The theme comes in rather than being held: panes change with the theme, and a pane that kept
    /// its own copy would keep drawing the old one after a switch.
    fn draw(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme);

    /// A key, when this pane has focus. The default is to ignore it: a pane with no input of its
    /// own (a log view) is a real thing, and ignoring is what lets the app's keys still work.
    fn on_key(&mut self, _key: KeyEvent) -> KeyOutcome {
        KeyOutcome::Ignored
    }

    /// Something happened in the engine — a model delta, a tool result, a file that changed.
    fn on_agent_event(&mut self, _event: &AgentEvent) {}

    /// A frame passed. Only panes with something to animate need this; it is the app that owns the
    /// clock, so that a pane never grows a timer of its own.
    fn tick(&mut self) {}

    /// What this pane wants sent to the engine, drained after every key.
    ///
    /// A pane does not talk to the model: it says what the human asked for, and the app decides
    /// what that means. Returning owned lines rather than holding a channel keeps a pane's
    /// behaviour a function of the keys it was given, which is what makes it testable.
    fn take_requests(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// Something happened to a file under the project root.
    ///
    /// A pane that shows files wants this; a pane that shows a conversation does not, and the
    /// default ignores it. It is told rather than asking, because asking means walking the tree on
    /// every frame, and the whole point of a watcher is that nobody has to.
    fn on_file_event(&mut self, _event: &jmds_core::event::FileEvent) {}

    /// Add a line from the app itself, for panes that have somewhere to put one.
    fn note(&mut self, _text: &str) {}

    /// Forget what is on screen, for panes that accumulate. What is on disk is not this method's
    /// business: clearing a view is not deleting a record.
    fn clear(&mut self) {}

    /// Where the terminal's cursor belongs, in `area`'s coordinates, if this pane has one.
    ///
    /// The pane cannot place it itself: the cursor is a property of the frame, not of a cell in the
    /// buffer, and drawing a fake one is how a text field ends up with two cursors that disagree.
    /// An editor's caret and an input line are the callers; a pane of read-only output has none.
    fn cursor(&self, _area: Rect) -> Option<Position> {
        None
    }
}

/// The panes, their layout, and the input that moves between them.
pub struct PaneHost {
    tree: PaneTree,
    panes: BTreeMap<PaneId, Box<dyn Pane>>,
    /// The geometry of the last frame, in the order it was drawn, in the engine's own rectangle
    /// type. Mouse hit-testing reads this rather than recomputing, so a click can never land in a
    /// pane the user is not looking at.
    geometry: Vec<(PaneId, PaneRect)>,
    /// One theme for the whole host, passed down to every pane as it is drawn.
    theme: Theme,
}

impl Default for PaneHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneHost {
    pub fn new() -> Self {
        Self::with_theme(Theme::default())
    }

    /// A host drawing with `theme`.
    pub fn with_theme(theme: Theme) -> Self {
        Self {
            tree: PaneTree::new(),
            panes: BTreeMap::new(),
            geometry: Vec::new(),
            theme,
        }
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Switch themes. Nothing has to be told: the next frame draws in the new one.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Open a pane, split off the focused one along `axis`.
    ///
    /// The first pane takes the whole area; the axis of a lone pane only matters once something
    /// else arrives.
    pub fn open(&mut self, axis: Axis, pane: impl Pane + 'static) -> PaneId {
        let id = self.tree.next_id();
        let placed = match self.tree.focused_id() {
            Some(near) if self.tree.contains(near) => self.tree.split(near, id, axis),
            _ => {
                self.tree.insert(id, axis);
                true
            }
        };
        if !placed {
            // Only reachable if the tree and this map disagree, which would be a bug in here. The
            // pane still exists, so it is still registered and still drawn when the tree agrees.
            log::warn!("pane {id} could not be placed in the tree");
        }
        self.panes.insert(id, Box::new(pane));
        self.tree.set_focus(id);
        id
    }

    /// Close a pane. Focus goes back to where it came from, which the tree decides.
    pub fn close(&mut self, id: PaneId) -> bool {
        if self.panes.remove(&id).is_none() {
            return false;
        }
        let closed = self.tree.close(id);
        self.geometry.retain(|(open, _)| *open != id);
        closed
    }

    pub fn tree(&self) -> &PaneTree {
        &self.tree
    }

    pub fn len(&self) -> usize {
        self.panes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub fn focused_id(&self) -> Option<PaneId> {
        self.tree.focused_id()
    }

    pub fn focus(&mut self, id: PaneId) -> bool {
        self.panes.contains_key(&id) && self.tree.set_focus(id)
    }

    pub fn cycle_focus(&mut self) -> Option<PaneId> {
        self.tree.cycle_focus()
    }

    pub fn focus_previous(&mut self) -> Option<PaneId> {
        if self.tree.focus_previous() {
            self.tree.focused_id()
        } else {
            None
        }
    }

    pub fn pane(&self, id: PaneId) -> Option<&dyn Pane> {
        self.panes.get(&id).map(|pane| &**pane)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut (dyn Pane + 'static)> {
        self.panes.get_mut(&id).map(|pane| &mut **pane)
    }

    /// The geometry of the last frame.
    pub fn geometry(&self) -> &[(PaneId, PaneRect)] {
        &self.geometry
    }

    /// Where the terminal's cursor belongs this frame, in screen cells.
    ///
    /// Only the focused pane is asked, and only when it has one: an unfocused editor's caret must
    /// not drag the cursor away from wherever the user is actually typing.
    pub fn focused_cursor(&self) -> Option<Position> {
        let id = self.tree.focused_id()?;
        let (_, rect) = self.geometry.iter().find(|(open, _)| *open == id)?;
        let inner = Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        };
        let position = self.panes.get(&id)?.cursor(inner)?;
        Some(Position::new(
            inner.x + position.x.min(inner.width.saturating_sub(1)),
            inner.y + position.y.min(inner.height.saturating_sub(1)),
        ))
    }

    /// Which pane holds `(column, row)` — what a click means.
    pub fn pane_at(&self, column: u16, row: u16) -> Option<PaneId> {
        self.geometry
            .iter()
            .find(|(_, rect)| rect.contains(column, row))
            .map(|(id, _)| *id)
    }

    /// Draw every pane, borders and titles included, and remember where each one landed.
    pub fn draw(&mut self, area: Rect, buf: &mut Buffer) {
        self.geometry = self.tree.rects(to_pane_rect(area));
        let focused = self.tree.focused_id();

        // Taken as a list first so one pane's mutable borrow does not live across another's.
        let styles = self.theme.styles();
        let geometry = std::mem::take(&mut self.geometry);
        for (id, rect) in &geometry {
            let Some(pane) = self.panes.get_mut(id) else {
                continue;
            };
            let style = if Some(*id) == focused {
                styles.border_focused
            } else {
                styles.border
            };
            // The focused pane's title is graded from the accent down to the muted colour: it is
            // the one piece of the frame that carries a colour ramp, and it is how the eye finds
            // the pane the keyboard is pointed at without a second border style.
            let title: Line<'static> = if Some(*id) == focused {
                effects::Ramp::new(self.theme.palette.accent, self.theme.palette.muted)
                    .spans(pane.title())
                    .into()
            } else {
                Line::from(pane.title().to_string())
            };
            let block = Block::bordered()
                .border_type(self.theme.glyphs.border())
                .title(title)
                .title_style(style)
                .border_style(style)
                .style(Style::default().fg(self.theme.palette.text));
            let rect = from_pane_rect(*rect);
            block.render(rect, buf);

            // A pane one cell wide has no inside. Drawing into nothing is not an error; it is what
            // a squeezed pane looks like.
            let inner = Rect {
                x: rect.x + 1,
                y: rect.y + 1,
                width: rect.width.saturating_sub(2),
                height: rect.height.saturating_sub(2),
            };
            if inner.width > 0 && inner.height > 0 {
                pane.draw(inner, buf, &self.theme);
            }
        }
        self.geometry = geometry;
    }

    /// Send a key to the focused pane.
    pub fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        let Some(id) = self.tree.focused_id() else {
            return KeyOutcome::Ignored;
        };
        match self.panes.get_mut(&id) {
            Some(pane) => pane.on_key(key),
            None => KeyOutcome::Ignored,
        }
    }

    /// A frame passed, for every pane.
    pub fn tick(&mut self) {
        for pane in self.panes.values_mut() {
            pane.tick();
        }
    }

    /// What the panes want sent, oldest first.
    pub fn take_requests(&mut self) -> Vec<String> {
        let mut requests = Vec::new();
        for pane in self.panes.values_mut() {
            requests.extend(pane.take_requests());
        }
        requests
    }

    /// The focused pane, for the app to talk to directly.
    pub fn focused_mut(&mut self) -> Option<&mut (dyn Pane + 'static)> {
        let id = self.focused_id()?;
        self.pane_mut(id)
    }

    /// Tell every pane what happened to a file.
    pub fn on_file_event(&mut self, event: &jmds_core::event::FileEvent) {
        for pane in self.panes.values_mut() {
            pane.on_file_event(event);
        }
    }

    /// Say something as the app, to whichever pane is focused.
    pub fn note(&mut self, text: &str) {
        if let Some(pane) = self.focused_mut() {
            pane.note(text);
        }
    }

    /// Clear whichever pane is focused, if it accumulates anything.
    pub fn clear(&mut self) {
        if let Some(pane) = self.focused_mut() {
            pane.clear();
        }
    }

    /// Tell every pane what happened. Panes that do not care ignore it by default, so this stays a
    /// straight fan-out rather than a routing table that has to be kept in step with reality.
    pub fn on_agent_event(&mut self, event: &AgentEvent) {
        for pane in self.panes.values_mut() {
            pane.on_agent_event(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use crossterm::event::{KeyCode, KeyModifiers};
    use jmds_core::pane::PaneKind;
    use ratatui::style::Color;

    use super::*;

    /// What a pane was asked to do, kept outside it so the host's promises are checked from the
    /// caller's side rather than by asking the pane whether it thinks it was called.
    #[derive(Clone, Default)]
    struct Notes {
        drawn: Rc<RefCell<Vec<Rect>>>,
        keys: Rc<RefCell<Vec<KeyCode>>>,
        events: Rc<RefCell<usize>>,
    }

    impl Notes {
        fn drawn(&self) -> Vec<Rect> {
            self.drawn.borrow().clone()
        }

        fn keys(&self) -> Vec<KeyCode> {
            self.keys.borrow().clone()
        }

        fn events(&self) -> usize {
            *self.events.borrow()
        }
    }

    struct Recorder {
        title: String,
        text: String,
        handles: bool,
        notes: Notes,
    }

    impl Recorder {
        fn new(title: &str, text: &str) -> (Self, Notes) {
            let notes = Notes::default();
            (
                Self {
                    title: title.to_string(),
                    text: text.to_string(),
                    handles: false,
                    notes: notes.clone(),
                },
                notes,
            )
        }
    }

    impl Pane for Recorder {
        fn kind(&self) -> PaneKind {
            PaneKind::Chat
        }

        fn title(&self) -> &str {
            &self.title
        }

        fn draw(&mut self, area: Rect, buf: &mut Buffer, _theme: &Theme) {
            self.notes.drawn.borrow_mut().push(area);
            buf.set_string(area.x, area.y, &self.text, Style::default());
        }

        fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
            self.notes.keys.borrow_mut().push(key.code);
            if self.handles {
                KeyOutcome::Handled
            } else {
                KeyOutcome::Ignored
            }
        }

        fn on_agent_event(&mut self, _event: &AgentEvent) {
            *self.notes.events.borrow_mut() += 1;
        }
    }

    fn whole() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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

    #[test]
    fn the_first_pane_fills_the_area_and_its_contents_land_inside_the_border() {
        let mut host = PaneHost::new();
        let (pane, notes) = Recorder::new("chat", "hello");
        host.open(Axis::Vertical, pane);

        let mut buf = Buffer::empty(whole());
        host.draw(whole(), &mut buf);

        assert_eq!(host.geometry().len(), 1);
        assert_eq!(
            host.geometry()[0].1,
            to_pane_rect(whole()),
            "one pane takes everything"
        );
        assert_eq!(
            notes.drawn(),
            vec![Rect {
                x: 1,
                y: 1,
                width: 38,
                height: 8
            }],
            "the pane draws inside the border, not over it"
        );
        let screen = screen(&buf);
        assert!(screen.contains("hello"), "{screen}");
        assert!(
            screen.contains("chat"),
            "the title is on the border: {screen}"
        );
    }

    #[test]
    fn a_second_pane_splits_the_area_and_both_are_drawn() {
        let mut host = PaneHost::new();
        let first = host.open(Axis::Horizontal, Recorder::new("chat", "left").0);
        let second = host.open(Axis::Horizontal, Recorder::new("files", "right").0);

        let mut buf = Buffer::empty(whole());
        host.draw(whole(), &mut buf);

        let geometry = host.geometry();
        assert_eq!(geometry.len(), 2);
        assert_eq!(geometry[0].0, first);
        assert_eq!(geometry[1].0, second);
        assert_eq!(geometry[0].1.width + geometry[1].1.width, whole().width);
        assert_eq!(geometry[1].1.x, geometry[0].1.x + geometry[0].1.width);

        let screen = screen(&buf);
        assert!(
            screen.contains("left") && screen.contains("right"),
            "{screen}"
        );
        assert!(
            screen.contains("chat") && screen.contains("files"),
            "{screen}"
        );
    }

    #[test]
    fn the_focused_title_is_graded_from_the_accent() {
        // The one colour ramp in the frame, and the only thing besides the border's brightness that
        // tells the eye where the keyboard is pointed.
        let mut host = PaneHost::new();
        host.open(Axis::Horizontal, Recorder::new("chat", "x").0);
        let focused = host.open(Axis::Horizontal, Recorder::new("files", "y").0);

        let mut buf = Buffer::empty(whole());
        host.draw(whole(), &mut buf);

        let geometry = host.geometry().to_vec();
        let rect_of = |id: PaneId| geometry.iter().find(|(open, _)| *open == id).unwrap().1;
        let title_colours = |rect: PaneRect, title: &str| {
            ((rect.x + 1)..(rect.x + 1 + title.len() as u16))
                .map(|x| buf[(x, rect.y)].fg)
                .collect::<Vec<Color>>()
        };

        let focused_title = title_colours(rect_of(focused), "files");
        assert_eq!(focused_title.len(), 5);
        assert_eq!(
            focused_title[0],
            Theme::default().palette.accent,
            "the focused title starts at the accent"
        );
        assert_eq!(
            focused_title[4],
            Theme::default().palette.muted,
            "and ends at the muted colour"
        );
        assert_ne!(focused_title[0], focused_title[4], "the ramp is flat");

        // An unfocused title is one colour: the ramp is what marks the focus, so half-applying it
        // would point at two panes at once.
        let other = title_colours(rect_of(geometry[0].0), "chat");
        assert!(
            other.windows(2).all(|pair| pair[0] == pair[1]),
            "the unfocused title is graded too: {other:?}"
        );
    }

    #[test]
    fn keys_reach_the_focused_pane_only() {
        let mut host = PaneHost::new();
        let (first_pane, first_notes) = Recorder::new("a", "a");
        let (second_pane, second_notes) = Recorder::new("b", "b");
        let first = host.open(Axis::Vertical, first_pane);
        let second = host.open(Axis::Vertical, second_pane);
        assert_eq!(
            host.focused_id(),
            Some(second),
            "focus follows the new pane"
        );

        host.on_key(key(KeyCode::Char('x')));
        assert_eq!(second_notes.keys(), vec![KeyCode::Char('x')]);
        assert!(first_notes.keys().is_empty());

        assert!(host.focus(first));
        host.on_key(key(KeyCode::Char('y')));
        assert_eq!(first_notes.keys(), vec![KeyCode::Char('y')]);
        assert_eq!(second_notes.keys().len(), 1);
    }

    #[test]
    fn a_pane_that_ignores_a_key_hands_it_back_to_the_app() {
        let mut host = PaneHost::new();
        host.open(Axis::Vertical, Recorder::new("a", "a").0);
        assert_eq!(host.on_key(key(KeyCode::Char('q'))), KeyOutcome::Ignored);

        let mut host = PaneHost::new();
        let mut pane = Recorder::new("a", "a").0;
        pane.handles = true;
        host.open(Axis::Vertical, pane);
        assert_eq!(host.on_key(key(KeyCode::Char('q'))), KeyOutcome::Handled);
    }

    #[test]
    fn every_pane_hears_what_the_engine_said() {
        let mut host = PaneHost::new();
        let (first_pane, first) = Recorder::new("a", "a");
        let (second_pane, second) = Recorder::new("b", "b");
        host.open(Axis::Vertical, first_pane);
        host.open(Axis::Vertical, second_pane);

        host.on_agent_event(&AgentEvent::Content("hi".into()));
        assert_eq!(first.events(), 1);
        assert_eq!(second.events(), 1);
    }

    #[test]
    fn closing_a_pane_gives_its_area_to_the_sibling() {
        let mut host = PaneHost::new();
        let first = host.open(Axis::Vertical, Recorder::new("a", "a").0);
        let second = host.open(Axis::Vertical, Recorder::new("b", "b").0);

        assert!(host.close(second));
        assert_eq!(host.len(), 1);
        assert_eq!(host.focused_id(), Some(first), "focus comes back");

        let mut buf = Buffer::empty(whole());
        host.draw(whole(), &mut buf);
        assert_eq!(host.geometry(), &[(first, to_pane_rect(whole()))]);
        assert!(host.pane(second).is_none());
    }

    #[test]
    fn panes_stack_when_the_axis_says_so() {
        // The two axes are not interchangeable: `Horizontal` is side by side, `Vertical` is
        // stacked, and a host that mixed them up would put the terminal somewhere else every time.
        let mut host = PaneHost::new();
        let first = host.open(Axis::Vertical, Recorder::new("a", "a").0);
        let second = host.open(Axis::Vertical, Recorder::new("b", "b").0);

        let mut buf = Buffer::empty(whole());
        host.draw(whole(), &mut buf);

        let geometry = host.geometry();
        assert_eq!(geometry[0].0, first);
        assert_eq!(geometry[1].0, second);
        assert_eq!(geometry[0].1.height + geometry[1].1.height, whole().height);
        assert_eq!(geometry[1].1.y, geometry[0].1.y + geometry[0].1.height);
        assert_eq!(geometry[0].1.width, whole().width);
    }

    #[test]
    fn a_click_lands_in_the_pane_that_was_drawn_there() {
        let mut host = PaneHost::new();
        let first = host.open(Axis::Horizontal, Recorder::new("a", "a").0);
        let second = host.open(Axis::Horizontal, Recorder::new("b", "b").0);

        let mut buf = Buffer::empty(whole());
        host.draw(whole(), &mut buf);

        // The geometry the host answers from is the geometry it drew with.
        let left = host.geometry()[0].1;
        let right = host.geometry()[1].1;
        assert_eq!(host.pane_at(left.x, left.y), Some(first));
        assert_eq!(
            host.pane_at(right.x + right.width - 1, right.y),
            Some(second)
        );
        assert_eq!(host.pane_at(whole().width, 0), None, "outside every pane");
    }

    #[test]
    fn the_two_rectangles_are_the_same_rectangle() {
        let area = Rect::new(3, 4, 20, 6);
        assert_eq!(from_pane_rect(to_pane_rect(area)), area);
        assert_eq!(to_pane_rect(area).x, 3);
    }

    #[test]
    fn a_pane_that_is_too_narrow_for_contents_is_still_placed() {
        let mut host = PaneHost::new();
        host.open(Axis::Horizontal, Recorder::new("a", "a").0);
        let (second_pane, second) = Recorder::new("b", "b");
        host.open(Axis::Horizontal, second_pane);

        // Two cells wide: a border, and no inside at all. The pane must not be asked to draw into
        // a zero-width area.
        let narrow = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 4,
        };
        // Split the width, so each half is one cell wide and neither has an inside.
        let mut buf = Buffer::empty(narrow);
        host.draw(narrow, &mut buf);
        assert_eq!(host.geometry().len(), 2);
        assert!(host.pane_at(0, 0).is_some());
        assert!(second.drawn().is_empty());
    }

    #[test]
    fn a_pane_that_was_closed_is_never_drawn_again() {
        let mut host = PaneHost::new();
        host.open(Axis::Vertical, Recorder::new("a", "a").0);
        let (second_pane, second) = Recorder::new("b", "b");
        let second_id = host.open(Axis::Vertical, second_pane);
        let mut buf = Buffer::empty(whole());
        host.draw(whole(), &mut buf);
        assert_eq!(second.drawn().len(), 1);

        host.close(second_id);
        host.draw(whole(), &mut buf);
        assert_eq!(second.drawn().len(), 1, "closed panes stay closed");
        assert_eq!(host.geometry().len(), 1);
    }
}
