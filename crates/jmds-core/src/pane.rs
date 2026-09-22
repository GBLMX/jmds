//! Where panes sit, as data.
//!
//! The behaviour — an editor buffer, a PTY, a chat transcript, and how each one draws — lives in
//! `jmds-tui` behind a `Pane` trait: drawing needs a terminal library and the engine must not
//! depend on one. What both sides agree on is here: an identity, a kind, a title, a size, and the
//! rules of the tree they live in.
//!
//! Panes the agent opens while it works are panes that have to be closed again, so "which pane has
//! the keyboard" has to be answerable in one place rather than by whichever task ran last.

use serde::{Deserialize, Serialize};

/// A pane's identity, stable while it is open.
///
/// A newtype rather than a bare `u64`: pane ids and session ids reach the same functions soon
/// enough, and swapping two integers is the kind of mistake that shows up as a pane closing
/// someone else's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PaneId(u64);

impl PaneId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The kinds of pane this app has — deliberately four. The workflow needs somewhere to talk,
/// somewhere to write the prompt files, a real shell, and a view of the tree; a fifth kind would
/// be a fifth thing to keep in the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    Chat,
    Editor,
    Terminal,
    FileTree,
}

impl PaneKind {
    pub const ALL: [Self; 4] = [Self::Chat, Self::Editor, Self::Terminal, Self::FileTree];

    /// Lowercase, as it reads in a title and in `config.toml`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Editor => "editor",
            Self::Terminal => "terminal",
            Self::FileTree => "files",
        }
    }
}

/// How much room a pane asks for.
///
/// [`Self::Percent`] is what a layout configured by hand wants; [`Self::Cells`] is what the agent
/// wants when it opens a terminal to show one command's output. The last pane in the tree takes
/// what is left rather than its own percentage, so a split always adds up to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSize {
    Percent(u16),
    Cells(u16),
}

impl PaneSize {
    /// `available` is how much the split has left. Zero is a real answer — a terminal too narrow
    /// for both panes — and a pane given zero cells draws nothing rather than panicking.
    pub const fn resolve(self, available: u16) -> u16 {
        match self {
            Self::Percent(percent) => {
                let percent = if percent > 100 { 100 } else { percent };
                ((available as u32 * percent as u32) / 100) as u16
            }
            Self::Cells(cells) => {
                if cells < available {
                    cells
                } else {
                    available
                }
            }
        }
    }
}

/// One pane, as the tree stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSpec {
    pub id: PaneId,
    pub kind: PaneKind,
    /// What the pane's border shows. A pane names itself once it knows what it holds (a terminal
    /// that ran `cargo test`, a chat pane after a model switch); this is what it shows until then.
    pub title: String,
    pub size: PaneSize,
}

impl PaneSpec {
    /// A pane with the default title for its kind.
    pub fn new(id: PaneId, kind: PaneKind, size: PaneSize) -> Self {
        Self {
            id,
            kind,
            title: kind.name().to_string(),
            size,
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }
}

/// The panes that exist, in layout order, and which one has focus.
///
/// Layout order is also the order the user reads them in: the tree and the chat come first because
/// they are always there, and a pane the agent opens lands at the end. Closing the focused pane
/// hands focus to whatever took its place, which is what makes "close this terminal" leave the
/// cursor somewhere predictable instead of at the start of the layout.
#[derive(Debug, Default, Clone)]
pub struct PaneTree {
    panes: Vec<PaneSpec>,
    focused: Option<PaneId>,
    /// The highest id handed out so far. Kept here rather than derived from `panes`, because the
    /// pane that carried it may already have closed — see [`Self::next_id`].
    next: u64,
}

impl PaneTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.panes.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &PaneSpec> {
        self.panes.iter()
    }

    pub fn get(&self, id: PaneId) -> Option<&PaneSpec> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    pub fn focused(&self) -> Option<&PaneSpec> {
        self.focused.and_then(|id| self.get(id))
    }

    pub fn focused_id(&self) -> Option<PaneId> {
        self.focused
    }

    /// Open a pane, or update the one that is already open under that id.
    ///
    /// Re-opening is how a pane is resized or retitled, so it keeps its place in the layout and
    /// keeps the keyboard if it had it: a title change that moved focus would be a surprise.
    pub fn open(&mut self, spec: PaneSpec) {
        // A pane restored from a session arrives with its own id; the counter has to stay ahead of
        // it, or the next pane opened would collide with a pane that already existed.
        self.next = self.next.max(spec.id.get());

        if let Some(existing) = self.panes.iter_mut().find(|pane| pane.id == spec.id) {
            *existing = spec;
        } else {
            self.panes.push(spec);
        }
        if self.focused.is_none() {
            self.focused = self.panes.last().map(|pane| pane.id);
        }
    }

    /// Close a pane and return what was closed.
    pub fn close(&mut self, id: PaneId) -> Option<PaneSpec> {
        let index = self.panes.iter().position(|pane| pane.id == id)?;
        let closed = self.panes.remove(index);

        if self.focused == Some(id) {
            // Whatever slid into that index, or the new last pane when the closed one was last.
            self.focused = self
                .panes
                .get(index)
                .or_else(|| self.panes.last())
                .map(|pane| pane.id);
        }

        Some(closed)
    }

    /// Give the keyboard to a pane. `false` when no such pane exists.
    pub fn focus(&mut self, id: PaneId) -> bool {
        if self.get(id).is_some() {
            self.focused = Some(id);
            true
        } else {
            false
        }
    }

    /// Move focus one pane along the layout, wrapping at the end.
    pub fn cycle_focus(&mut self) -> Option<PaneId> {
        if self.panes.is_empty() {
            return None;
        }
        let next = match self
            .focused
            .and_then(|id| self.panes.iter().position(|p| p.id == id))
        {
            Some(index) => (index + 1) % self.panes.len(),
            None => 0,
        };
        self.focused = Some(self.panes[next].id);
        self.focused
    }

    /// The next unused id.
    ///
    /// Ids are never reused. A pane that closed is still named in the transcript and in the log,
    /// and an id that came to mean a *different* pane is worse than a gap in the numbering — which
    /// is why the counter lives in the tree and not in `panes`: emptying the tree must not restart
    /// the numbering at one.
    pub fn next_id(&mut self) -> PaneId {
        self.next += 1;
        PaneId::new(self.next)
    }

    pub fn of_kind(&self, kind: PaneKind) -> impl Iterator<Item = &PaneSpec> {
        self.panes.iter().filter(move |pane| pane.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: u64, kind: PaneKind) -> PaneSpec {
        PaneSpec::new(PaneId::new(id), kind, PaneSize::Percent(50))
    }

    #[test]
    fn a_percentage_that_exceeds_the_split_is_clamped() {
        assert_eq!(PaneSize::Percent(50).resolve(80), 40);
        assert_eq!(PaneSize::Percent(150).resolve(80), 80);
        assert_eq!(PaneSize::Percent(0).resolve(80), 0);
    }

    #[test]
    fn a_fixed_width_is_capped_by_what_is_available() {
        assert_eq!(PaneSize::Cells(20).resolve(80), 20);
        assert_eq!(PaneSize::Cells(80).resolve(80), 80);
        assert_eq!(PaneSize::Cells(200).resolve(80), 80);
    }

    #[test]
    fn the_first_pane_takes_focus_and_later_ones_do_not_steal_it() {
        let mut tree = PaneTree::new();
        tree.open(spec(1, PaneKind::FileTree));
        assert_eq!(tree.focused_id(), Some(PaneId::new(1)));
        tree.open(spec(2, PaneKind::Chat));
        // Opening a pane must not take the keyboard away from what the user was typing into.
        assert_eq!(tree.focused_id(), Some(PaneId::new(1)));
    }

    #[test]
    fn opening_an_id_again_updates_it_in_place() {
        let mut tree = PaneTree::new();
        tree.open(spec(1, PaneKind::Chat));
        tree.open(spec(1, PaneKind::Chat).with_title("deepseek-chat"));

        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get(PaneId::new(1)).unwrap().title, "deepseek-chat");
    }

    #[test]
    fn closing_the_focused_pane_focuses_the_one_that_took_its_place() {
        let mut tree = PaneTree::new();
        for id in 1..=3 {
            tree.open(spec(id, PaneKind::Terminal));
        }
        tree.focus(PaneId::new(2)).then_some(()).unwrap();
        tree.close(PaneId::new(2));
        assert_eq!(tree.focused_id(), Some(PaneId::new(3)));

        // Closing the last one has nothing after it, so focus falls back to what is there.
        tree.close(PaneId::new(3));
        assert_eq!(tree.focused_id(), Some(PaneId::new(1)));
    }

    #[test]
    fn closing_the_only_pane_leaves_nothing_focused() {
        let mut tree = PaneTree::new();
        tree.open(spec(1, PaneKind::Editor));
        assert!(tree.close(PaneId::new(1)).is_some());
        assert!(tree.is_empty());
        assert_eq!(tree.focused_id(), None);
        assert!(tree.close(PaneId::new(1)).is_none());
    }

    #[test]
    fn focus_cycles_through_the_layout_and_wraps() {
        let mut tree = PaneTree::new();
        for id in 1..=3 {
            tree.open(spec(id, PaneKind::Terminal));
        }
        tree.focus(PaneId::new(1)).then_some(()).unwrap();
        assert_eq!(tree.cycle_focus(), Some(PaneId::new(2)));
        assert_eq!(tree.cycle_focus(), Some(PaneId::new(3)));
        assert_eq!(tree.cycle_focus(), Some(PaneId::new(1)));
    }

    #[test]
    fn ids_are_never_reused_not_even_after_the_tree_empties() {
        let mut tree = PaneTree::new();
        let first = tree.next_id();
        tree.open(spec(first.get(), PaneKind::Chat));
        tree.close(first);

        assert!(tree.is_empty());
        assert_ne!(
            tree.next_id(),
            first,
            "an id that outlives its pane must not come to mean a new one"
        );
    }

    #[test]
    fn a_pane_that_arrives_with_its_own_id_keeps_the_counter_ahead_of_it() {
        // How a session restores panes: the id is already in the transcript.
        let mut tree = PaneTree::new();
        tree.open(spec(7, PaneKind::Terminal));
        assert_eq!(tree.next_id(), PaneId::new(8));
    }

    #[test]
    fn focusing_a_pane_that_is_not_open_is_refused() {
        let mut tree = PaneTree::new();
        tree.open(spec(1, PaneKind::Chat));
        assert!(!tree.focus(PaneId::new(7)));
        assert_eq!(tree.focused_id(), Some(PaneId::new(1)));
    }
}
