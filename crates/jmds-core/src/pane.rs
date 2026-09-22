//! Where panes sit, as data.
//!
//! The tree holds **only ids**: an identity and a place, nothing else. What a pane *is* (its kind,
//! its title) lives in the registry the caller keeps, and what a pane *does* (an editor buffer, a
//! PTY, a chat transcript, and how each draws) lives in `jmds-tui` behind a `Pane` trait —
//! drawing needs a terminal library and the engine must not depend on one.
//!
//! A binary space partition rather than a flat list of panes: `PaneNode::Split` holds the axis and
//! the ratio, so a rectangle falls out of the tree arithmetically, closing a pane gives its area
//! to the sibling it shared a split with, and ratios survive a restart. That shape is borrowed
//! from how terminal multiplexers do it (see the reference notes), and it is what makes "the agent
//! opens a terminal next to the editor" a tree edit rather than a re-layout.

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
/// somewhere to write the prompt files, a real shell, and a view of the tree.
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

    /// The kind a name in `config.toml` refers to.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// One pane's identity and what it calls itself. Geometry is the tree's business.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSpec {
    pub id: PaneId,
    pub kind: PaneKind,
    /// What the border shows. A pane renames itself once it knows what it holds (a terminal that
    /// ran `cargo test`, a chat pane after a model switch) via the `Title` event; this is what it
    /// shows until then.
    pub title: String,
}

impl PaneSpec {
    /// A pane with the default title for its kind.
    pub fn new(id: PaneId, kind: PaneKind) -> Self {
        Self {
            id,
            kind,
            title: kind.name().to_string(),
        }
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }
}

/// Which way a split divides its area.
///
/// `Horizontal` puts the two children side by side and divides the *width* — the name follows
/// ratatui's `Direction`, where a horizontal layout runs left to right. Mixing the two up is the
/// classic split bug, so the variant names say where the children go, not where the line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// A rectangle in cells, relative to the area handed to [`PaneTree::rects`].
///
/// `jmds-core` has its own rectangle instead of borrowing ratatui's: the engine must not depend on
/// a terminal library, and this is four integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Whether a cell is inside.
    ///
    /// The right and bottom edges are outside: `x + width` is the first column the rectangle does
    /// not cover, the same half-open rule the split arithmetic follows. Written with `wrapping_sub`
    /// so a cell far to the left or above cannot overflow into a false hit.
    pub const fn contains(self, x: u16, y: u16) -> bool {
        x.wrapping_sub(self.x) < self.width && y.wrapping_sub(self.y) < self.height
    }

    /// The area a pane covers, as `(x, y, width, height)`, for a test that needs to see it.
    pub const fn area(self) -> u32 {
        self.width as u32 * self.height as u32
    }

    fn split(self, axis: Axis, ratio: f32) -> (Self, Self) {
        match axis {
            Axis::Horizontal => {
                let first = ((self.width as f32 * ratio).round() as u16).min(self.width);
                (
                    Self::new(self.x, self.y, first, self.height),
                    Self::new(self.x + first, self.y, self.width - first, self.height),
                )
            }
            Axis::Vertical => {
                let first = ((self.height as f32 * ratio).round() as u16).min(self.height);
                (
                    Self::new(self.x, self.y, self.width, first),
                    Self::new(self.x, self.y + first, self.width, self.height - first),
                )
            }
        }
    }
}

/// One node of the split tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PaneNode {
    Leaf(PaneId),
    Split {
        axis: Axis,
        /// How much of the axis the first child gets. Kept in the tree (and persisted) so a
        /// resize survives a restart, and clamped so neither child can be squeezed to nothing.
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { first, second, .. } => {
                first.leaves(out);
                second.leaves(out);
            }
        }
    }

    fn contains(&self, id: PaneId) -> bool {
        match self {
            Self::Leaf(leaf) => *leaf == id,
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    fn rects(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Self::Leaf(id) => out.push((*id, area)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = area.split(*axis, *ratio);
                first.rects(a, out);
                second.rects(b, out);
            }
        }
    }

    /// Remove `id` from this subtree.
    ///
    /// `None` means *this subtree was the pane*: the parent has to replace the whole node with the
    /// sibling, which is how a closed pane's area goes to whoever it shared a split with instead of
    /// leaving a hole.
    fn remove(self, id: PaneId) -> Option<PaneNode> {
        match self {
            Self::Leaf(leaf) => (leaf != id).then_some(Self::Leaf(leaf)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                if first.contains(id) {
                    match first.remove(id) {
                        // The first branch *was* the pane: this split collapses into its sibling,
                        // which is how the closed pane's area is handed over instead of left empty.
                        None => Some(*second),
                        Some(first) => Some(Self::Split {
                            axis,
                            ratio,
                            first: Box::new(first),
                            second,
                        }),
                    }
                } else if second.contains(id) {
                    match second.remove(id) {
                        None => Some(*first),
                        Some(second) => Some(Self::Split {
                            axis,
                            ratio,
                            first,
                            second: Box::new(second),
                        }),
                    }
                } else {
                    Some(Self::Split {
                        axis,
                        ratio,
                        first,
                        second,
                    })
                }
            }
        }
    }

    /// Set the ratio of the *deepest* split that still contains `id` — the one between it and its
    /// immediate neighbour, which is the one a drag on that border means.
    fn set_ratio(&mut self, id: PaneId, ratio: f32) -> bool {
        let Self::Split {
            ratio: current,
            first,
            second,
            ..
        } = self
        else {
            return false;
        };
        if !(first.contains(id) || second.contains(id)) {
            return false;
        }
        if first.set_ratio(id, ratio) || second.set_ratio(id, ratio) {
            return true;
        }
        *current = clamp_ratio(ratio);
        true
    }
}

/// Neither child may be squeezed out of existence: a split that reads as "closed" is a surprise,
/// and 10% is already narrow enough to be unusable for an editor.
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

fn clamp_ratio(ratio: f32) -> f32 {
    if !ratio.is_finite() {
        return 0.5;
    }
    ratio.clamp(MIN_RATIO, MAX_RATIO)
}

/// The panes that exist, how they are split, and which one has the keyboard.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PaneTree {
    root: Option<PaneNode>,
    focus: Option<PaneId>,
    /// Where focus came from. Closing the focused pane hands the keyboard back here first, which is
    /// what makes "close this terminal" return the user to what they were writing in.
    prev_focus: Option<PaneId>,
    next: u64,
}

impl PaneTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn len(&self) -> usize {
        self.leaves().len()
    }

    /// Every pane id, in the order they appear in the split tree (left to right, top to bottom).
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.leaves(&mut out);
        }
        out
    }

    pub fn contains(&self, id: PaneId) -> bool {
        self.root.as_ref().is_some_and(|root| root.contains(id))
    }

    pub fn focused_id(&self) -> Option<PaneId> {
        self.focus
    }

    pub fn previous_focus(&self) -> Option<PaneId> {
        self.prev_focus
    }

    /// The next unused id.
    ///
    /// Ids are never reused: a pane that closed is still named in the transcript and in the log,
    /// and an id that came to mean a *different* pane is worse than a gap in the numbering — which
    /// is why the counter lives here and not in the tree: emptying it must not restart at one.
    pub fn next_id(&mut self) -> PaneId {
        self.next += 1;
        PaneId::new(self.next)
    }

    /// Keep the counter ahead of an id that arrived from outside (a restored session).
    pub fn reserve(&mut self, id: PaneId) {
        self.next = self.next.max(id.get());
    }

    /// Put the first pane in, or add another one beside the whole tree.
    ///
    /// An empty tree adopts the pane; a tree with panes in it splits the root, which is what
    /// "open another pane" means when there is no pane to split *near*.
    pub fn insert(&mut self, id: PaneId, axis: Axis) {
        self.reserve(id);
        self.root = match self.root.take() {
            None => Some(PaneNode::Leaf(id)),
            Some(root) => Some(PaneNode::Split {
                axis,
                ratio: 0.5,
                first: Box::new(root),
                second: Box::new(PaneNode::Leaf(id)),
            }),
        };
        if self.focus.is_none() {
            self.focus = Some(id);
        }
    }

    /// Split the pane `near` in two, putting `id` in the new half.
    ///
    /// `false` when `near` is not open. Focus follows the new pane: the agent opened it to show
    /// something, and the user asked for the split.
    pub fn split(&mut self, near: PaneId, id: PaneId, axis: Axis) -> bool {
        if !self.contains(near) {
            return false;
        }
        self.reserve(id);

        fn split_at(node: &mut PaneNode, near: PaneId, id: PaneId, axis: Axis) -> bool {
            match node {
                PaneNode::Leaf(leaf) if *leaf == near => {
                    let old = PaneNode::Leaf(*leaf);
                    *node = PaneNode::Split {
                        axis,
                        ratio: 0.5,
                        first: Box::new(old),
                        second: Box::new(PaneNode::Leaf(id)),
                    };
                    true
                }
                PaneNode::Leaf(_) => false,
                PaneNode::Split { first, second, .. } => {
                    split_at(first, near, id, axis) || split_at(second, near, id, axis)
                }
            }
        }

        if split_at(
            self.root.as_mut().expect("contains() implies a root"),
            near,
            id,
            axis,
        ) {
            self.set_focus(id);
            true
        } else {
            false
        }
    }

    /// Close a pane and hand its area to the sibling it shared a split with.
    pub fn close(&mut self, id: PaneId) -> bool {
        let Some(root) = self.root.take() else {
            return false;
        };
        if !root.contains(id) {
            self.root = Some(root);
            return false;
        }

        // Focus is decided against the layout *before* the removal: "the pane after the one that
        // closed" is a fact about where it was, not about where its sibling ended up.
        let order = {
            let mut out = Vec::new();
            root.leaves(&mut out);
            out
        };

        match root.remove(id) {
            None => {
                self.root = None;
                self.focus = None;
                self.prev_focus = None;
            }
            Some(root) => {
                self.root = Some(root);
                self.focus = self.focus_after_close(id, &order);
                if self.prev_focus == Some(id) {
                    self.prev_focus = None;
                }
            }
        }
        true
    }

    /// Where the keyboard goes after `closed` was removed: back where focus came from, else the
    /// pane that followed it, else the one before it.
    fn focus_after_close(&self, closed: PaneId, order_before: &[PaneId]) -> Option<PaneId> {
        if let Some(previous) = self.prev_focus
            && previous != closed
            && self.contains(previous)
        {
            return Some(previous);
        }
        let index = order_before.iter().position(|id| *id == closed)?;
        order_before
            .iter()
            .skip(index + 1)
            .chain(order_before.iter().take(index).rev())
            .find(|id| self.contains(**id))
            .copied()
    }

    /// Move the keyboard to a pane. `false` when it is not open.
    pub fn set_focus(&mut self, id: PaneId) -> bool {
        if !self.contains(id) {
            return false;
        }
        if self.focus != Some(id) {
            self.prev_focus = self.focus;
            self.focus = Some(id);
        }
        true
    }

    /// Hand the keyboard back to the pane it came from.
    pub fn focus_previous(&mut self) -> bool {
        match self.prev_focus.filter(|id| self.contains(*id)) {
            Some(previous) => {
                let current = self.focus;
                self.focus = Some(previous);
                self.prev_focus = current;
                true
            }
            None => false,
        }
    }

    /// Move focus to the next pane in layout order, wrapping at the end.
    pub fn cycle_focus(&mut self) -> Option<PaneId> {
        let order = self.leaves();
        if order.is_empty() {
            return None;
        }
        let next = match self
            .focus
            .and_then(|id| order.iter().position(|x| *x == id))
        {
            Some(index) => (index + 1) % order.len(),
            None => 0,
        };
        self.set_focus(order[next]);
        self.focus
    }

    /// Set the ratio of the split that holds `id`. Clamped, so no pane can be squeezed away.
    pub fn set_ratio(&mut self, id: PaneId, ratio: f32) -> bool {
        match self.root.as_mut() {
            Some(root) => root.set_ratio(id, ratio),
            None => false,
        }
    }

    /// The ratio of the split that holds `id`, if it is in one.
    pub fn ratio_of(&self, id: PaneId) -> Option<f32> {
        fn walk(node: &PaneNode, id: PaneId, found: &mut Option<f32>) {
            if let PaneNode::Split {
                ratio,
                first,
                second,
                ..
            } = node
            {
                if first.contains(id) || second.contains(id) {
                    *found = Some(*ratio);
                }
                walk(first, id, found);
                walk(second, id, found);
            }
        }
        let mut found = None;
        if let Some(root) = &self.root {
            walk(root, id, &mut found);
        }
        found
    }

    /// Where every pane goes inside `area`, in layout order.
    ///
    /// The rectangles tile the area exactly: the split arithmetic takes the first child's size
    /// from the ratio and gives the second the remainder, so no cell is lost or drawn twice.
    pub fn rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.rects(area, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> PaneId {
        PaneId::new(n)
    }

    /// The invariant every layout has to hold: the panes cover the area and never overlap it.
    fn assert_tiles(tree: &PaneTree, area: Rect) {
        let rects = tree.rects(area);
        assert_eq!(rects.len(), tree.len());
        let covered: u32 = rects.iter().map(|(_, r)| r.area()).sum();
        assert_eq!(
            covered,
            area.area(),
            "rects must tile the area exactly: {rects:?}"
        );
        for (i, (_, a)) in rects.iter().enumerate() {
            assert!(
                a.x + a.width <= area.x + area.width,
                "width overflow: {a:?}"
            );
            assert!(
                a.y + a.height <= area.y + area.height,
                "height overflow: {a:?}"
            );
            for (_, b) in rects.iter().skip(i + 1) {
                let disjoint = a.x + a.width <= b.x
                    || b.x + b.width <= a.x
                    || a.y + a.height <= b.y
                    || b.y + b.height <= a.y;
                assert!(disjoint, "panes overlap: {a:?} and {b:?}");
            }
        }
    }

    #[test]
    fn a_new_tree_has_one_pane_covering_everything() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        assert_eq!(tree.leaves(), vec![id(1)]);
        assert_eq!(tree.focused_id(), Some(id(1)));
        assert_eq!(
            tree.rects(Rect::new(0, 0, 80, 24)),
            vec![(id(1), Rect::new(0, 0, 80, 24))]
        );
    }

    #[test]
    fn a_split_divides_the_width_or_the_height() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);

        let rects = tree.rects(area);
        assert_eq!(rects[0], (id(1), Rect::new(0, 0, 40, 24)));
        assert_eq!(rects[1], (id(2), Rect::new(40, 0, 40, 24)));
        assert_tiles(&tree, area);

        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Vertical);
        tree.split(id(1), id(2), Axis::Vertical);
        let rects = tree.rects(area);
        assert_eq!(rects[0], (id(1), Rect::new(0, 0, 80, 12)));
        assert_eq!(rects[1], (id(2), Rect::new(0, 12, 80, 12)));
        assert_tiles(&tree, area);
    }

    #[test]
    fn splitting_the_root_again_nests_inside_the_existing_split() {
        let area = Rect::new(0, 0, 90, 30);
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);
        tree.split(id(1), id(3), Axis::Vertical);

        // 3 is a vertical split of the left half only.
        let rects = tree.rects(area);
        assert_tiles(&tree, area);
        // A split keeps the pane that was here first and gives the new one the second half, so
        // 3 — opened by splitting 1 vertically — lands *below* 1, inside the left column only.
        assert!(
            rects.contains(&(id(1), Rect::new(0, 0, 45, 15))),
            "{rects:?}"
        );
        assert!(
            rects.contains(&(id(3), Rect::new(0, 15, 45, 15))),
            "{rects:?}"
        );
        assert!(
            rects.contains(&(id(2), Rect::new(45, 0, 45, 30))),
            "{rects:?}"
        );
    }

    #[test]
    fn closing_a_pane_gives_its_area_to_the_sibling_it_shared_a_split_with() {
        let area = Rect::new(0, 0, 80, 24);
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);

        assert!(tree.close(id(1)));
        assert_eq!(tree.leaves(), vec![id(2)]);
        assert_eq!(
            tree.rects(area),
            vec![(id(2), area)],
            "the survivor takes the whole split, not half of it"
        );
        assert_tiles(&tree, area);
    }

    #[test]
    fn a_pane_that_arrives_with_its_own_id_keeps_the_counter_ahead_of_it() {
        let mut tree = PaneTree::new();
        tree.insert(id(7), Axis::Horizontal); // how a session restore would
        assert_eq!(tree.next_id(), id(8));
    }

    #[test]
    fn ids_are_never_reused_not_even_after_the_tree_empties() {
        let mut tree = PaneTree::new();
        let first = tree.next_id();
        tree.insert(first, Axis::Horizontal);
        tree.close(first);

        assert!(tree.is_empty());
        assert_ne!(
            tree.next_id(),
            first,
            "an id that outlives its pane must not come to mean a new one"
        );
    }

    #[test]
    fn focus_goes_to_the_new_pane_and_returns_to_the_previous_one_when_it_closes() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);

        // `split` moved focus to the new pane, remembering where it came from.
        assert_eq!(tree.focused_id(), Some(id(2)));
        assert_eq!(tree.previous_focus(), Some(id(1)));

        tree.close(id(2));
        assert_eq!(
            tree.focused_id(),
            Some(id(1)),
            "closing the pane the agent opened puts the user back where they were"
        );
    }

    #[test]
    fn closing_a_pane_with_no_previous_focus_uses_layout_order() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);
        tree.split(id(2), id(3), Axis::Horizontal);
        tree.set_focus(id(3));
        tree.set_focus(id(2)); // prev_focus is 3 … which we then remove, so it must fall back
        tree.close(id(3));

        assert!(tree.contains(id(2)));
        assert_eq!(tree.focused_id(), Some(id(2)));
    }

    #[test]
    fn closing_the_focused_pane_prefers_where_focus_came_from() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);
        tree.split(id(2), id(3), Axis::Horizontal);

        tree.set_focus(id(1));
        tree.set_focus(id(3));
        assert_eq!(tree.previous_focus(), Some(id(1)));

        tree.close(id(3));
        assert_eq!(tree.focused_id(), Some(id(1)));
    }

    #[test]
    fn a_ratio_is_clamped_so_no_pane_can_be_squeezed_away() {
        let area = Rect::new(0, 0, 100, 20);
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);

        assert!(tree.set_ratio(id(2), 0.99));
        assert_eq!(tree.ratio_of(id(2)), Some(MAX_RATIO));
        assert!(tree.set_ratio(id(2), -3.0));
        assert_eq!(tree.ratio_of(id(2)), Some(MIN_RATIO));

        let rects = tree.rects(area);
        assert!(rects.iter().all(|(_, r)| r.width >= 10), "{rects:?}");
        assert_tiles(&tree, area);
    }

    #[test]
    fn a_ratio_that_is_not_a_number_falls_back_to_the_middle() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);
        tree.set_ratio(id(2), f32::NAN);
        assert_eq!(tree.ratio_of(id(2)), Some(0.5));
    }

    #[test]
    fn focus_cycles_through_layout_order_and_wraps() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);
        tree.split(id(2), id(3), Axis::Horizontal);

        tree.set_focus(id(1));
        assert_eq!(tree.cycle_focus(), Some(id(2)));
        assert_eq!(tree.cycle_focus(), Some(id(3)));
        assert_eq!(tree.cycle_focus(), Some(id(1)));
    }

    #[test]
    fn focus_previous_swaps_back_and_forth() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        tree.split(id(1), id(2), Axis::Horizontal);
        tree.set_focus(id(1));

        assert!(tree.focus_previous());
        assert_eq!(tree.focused_id(), Some(id(2)));
        assert!(tree.focus_previous());
        assert_eq!(tree.focused_id(), Some(id(1)));
    }

    #[test]
    fn splitting_and_focusing_something_that_is_not_open_is_refused() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        assert!(!tree.split(id(9), id(2), Axis::Horizontal));
        assert!(!tree.set_focus(id(9)));
        assert!(!tree.close(id(9)));
        assert!(!tree.set_ratio(id(9), 0.5));
        assert_eq!(tree.leaves(), vec![id(1)]);
    }

    #[test]
    fn a_rectangle_holds_the_cells_inside_it_and_not_its_edges() {
        let rect = Rect::new(2, 3, 4, 5);
        assert!(rect.contains(2, 3), "the top left corner is inside");
        assert!(rect.contains(5, 7), "the last cell is inside");
        assert!(!rect.contains(6, 7), "the right edge is outside");
        assert!(!rect.contains(5, 8), "the bottom edge is outside");
        assert!(!rect.contains(1, 3) && !rect.contains(2, 2));
        assert!(
            !rect.contains(u16::MAX, u16::MAX),
            "far away cannot wrap in"
        );
        assert!(!rect.contains(0, 0));
    }

    #[test]
    fn an_empty_tree_has_no_rects_focus_or_ratios() {
        let mut tree = PaneTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.focused_id(), None);
        assert!(tree.cycle_focus().is_none());
        assert!(tree.rects(Rect::new(0, 0, 10, 10)).is_empty());
        assert!(!tree.set_ratio(id(1), 0.5));
    }

    #[test]
    fn closing_the_last_pane_leaves_an_empty_tree() {
        let mut tree = PaneTree::new();
        tree.insert(id(1), Axis::Horizontal);
        assert!(tree.close(id(1)));
        assert!(tree.is_empty());
        assert_eq!(tree.focused_id(), None);
        assert_eq!(tree.previous_focus(), None);
    }

    #[test]
    fn every_kind_round_trips_through_its_config_name() {
        for kind in PaneKind::ALL {
            assert_eq!(PaneKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(PaneKind::from_name("nothing"), None);
    }

    #[test]
    fn a_pane_takes_its_kind_as_its_default_title() {
        let spec = PaneSpec::new(id(1), PaneKind::FileTree);
        assert_eq!(spec.title, "files");
        assert_eq!(spec.with_title("src/").title, "src/");
    }
}
