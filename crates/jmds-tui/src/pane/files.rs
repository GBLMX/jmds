//! The file tree: the session's working directory, a layer at a time.
//!
//! Three decisions worth naming:
//!
//! - **The disk is read one layer at a time.** [`FileTree::refresh`] and the splice under it read
//!   the root and the folders that are open, and nothing else: a folded folder is a row and no more.
//!   Reading the whole tree up front would spend the first frame of every project inside `target/`,
//!   and everything it read would go stale the moment a build wrote into it.
//! - **The clock is the app's.** A change is marked for [`RECENT_TICKS`] *ticks*, not for a wall
//!   second: the host calls [`Pane::tick`] once a frame, so the mark fades by counting, this pane
//!   stays a function of the state it was given, and a test advances two seconds by calling `tick`
//!   twenty-six times rather than by sleeping.
//! - **Every order is decided here.** Directories first, then by name; the expansion set is a
//!   `BTreeSet`. `read_dir` yields the file system's order and a `HashMap` yields history's, and a
//!   tree that reshuffles between frames is one nobody can aim at.
//!
//! What this pane is not: an editor. `→` on a file does nothing, because opening a file is another
//! pane's job and a tree that also opened files would be two answers to one key.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jmds_core::{event::FileEvent, pane::PaneKind};
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};

use super::{KeyOutcome, Pane};
use crate::theme::Theme;

/// How long a file stays marked as just-changed, in ticks.
///
/// The app ticks every 80ms, so this is about two seconds: long enough to spot the file a tool wrote
/// while the answer above it is being read, short enough that a build does not leave every row lit.
const RECENT_TICKS: u64 = 25;

/// The cells one level of depth indents by.
const INDENT: u16 = 2;

/// What a row stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Directory,
    File,
    /// A sentence where a folder's contents would be — empty, or unreadable.
    Note,
}

/// One row of the tree.
///
/// Whether the row is marked as recently changed is *not* a field: it is a lookup in
/// [`FileTree::recent`] when the row is drawn. A flag would need clearing when the mark ages out,
/// which is a second place that knows when a mark is over.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    /// Absolute. For a note it is the folder the note is about, so `←` has somewhere to go.
    path: PathBuf,
    /// The file name; for a note, the sentence itself.
    label: String,
    /// How deep, for the indentation. The root is 0, its children are 1.
    depth: usize,
    kind: NodeKind,
    /// Whether this folder's children are on screen. Only ever true for a folder that is open.
    expanded: bool,
}

impl Node {
    /// A line of explanation under `directory`, standing in for a layer with nothing to show.
    fn note(directory: &Path, depth: usize, text: String) -> Self {
        Self {
            path: directory.to_path_buf(),
            label: text,
            depth,
            kind: NodeKind::Note,
            expanded: false,
        }
    }
}

/// The tree, its window, and what changed lately.
pub struct FileTree {
    /// The session's working directory: the one folder that cannot be folded away.
    root: PathBuf,
    /// Which folders are open. A `BTreeSet` because it decides the order of what is read, and the
    /// order of what is read is the order on screen.
    expanded: BTreeSet<PathBuf>,
    /// The rows on screen, in drawing order — the root and the open folders' own layers, nothing
    /// deeper. A window over the tree, not the tree.
    entries: Vec<Node>,
    /// The row the keyboard is on, as an index into `entries`.
    selected: usize,
    /// The first row drawn. The window follows the selection; `draw` is the only place that knows
    /// how many rows fit, so that is where this is reconciled.
    top: usize,
    /// When each path was last seen changed, in ticks, keyed by absolute path. Only paths under the
    /// root: the watcher watches a project, and this pane shows one directory of it.
    recent: BTreeMap<PathBuf, u64>,
    /// Frames since this pane was opened. The whole clock it has.
    tick: u64,
}

impl FileTree {
    /// A tree rooted at `root`, with the root's own layer already listed.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let mut tree = Self {
            root: root.into(),
            expanded: BTreeSet::new(),
            entries: Vec::new(),
            selected: 0,
            top: 0,
            recent: BTreeMap::new(),
            tick: 0,
        };
        tree.refresh();
        tree
    }

    /// The folder this tree is rooted at — the session's working directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Follow the session to another working directory.
    ///
    /// Re-rooting forgets the open folders and the recent marks: which folders were open describes a
    /// tree that is not on screen any more, and a path that was just written in the old project says
    /// nothing about the new one. Telling it where it already is does *nothing at all*, because a
    /// host may hand over the current directory every frame and that must not close what a person
    /// opened.
    pub fn set_root(&mut self, root: impl Into<PathBuf>) {
        let root = root.into();
        if root == self.root {
            return;
        }
        self.root = root;
        self.expanded.clear();
        self.recent.clear();
        self.selected = 0;
        self.top = 0;
        self.refresh();
    }

    /// Read the root and every open folder, in order, and take the result as the whole tree.
    ///
    /// This is `r`, and it is also what a re-root does. It reads no folder that is closed.
    pub fn refresh(&mut self) {
        let mut rows = vec![Node {
            path: self.root.clone(),
            label: self.root_name(),
            depth: 0,
            kind: NodeKind::Directory,
            expanded: true,
        }];
        collect(&self.root, 1, &self.expanded, &mut rows);
        self.entries = rows;
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.top = self.top.min(self.selected);
    }

    /// The root's own name, for the first row. A root with no name of its own (`/`) is its path.
    fn root_name(&self) -> String {
        self.root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    /// Whether a folder's children are on screen. The root always is: a tree that showed nothing
    /// would be a pane with nothing in it.
    fn is_open(&self, directory: &Path) -> bool {
        directory == self.root || self.expanded.contains(directory)
    }

    /// Fold a folder open or shut, and show the difference.
    fn set_expanded(&mut self, directory: &Path, open: bool) {
        if open {
            self.expanded.insert(directory.to_path_buf());
        } else {
            self.expanded.remove(directory);
        }
        self.splice(directory);
    }

    /// Re-read one folder's own layer and put the result under its row.
    ///
    /// Targeted rather than a full refresh: opening or closing a folder is about that folder, and
    /// re-reading every other open folder to show one folder's children is work whose answer nobody
    /// asked for. Only open layers are ever read, so the cost is the rows on screen and not the size
    /// of the tree. A folder that is not a row here is not on screen, so there is nothing to change.
    fn splice(&mut self, directory: &Path) {
        let Some(at) = self.entries.iter().position(|row| row.path == directory) else {
            return;
        };
        let depth = self.entries[at].depth;
        // Whatever hangs under this row now: the deeper rows up to the next sibling.
        let mut end = at + 1;
        while end < self.entries.len() && self.entries[end].depth > depth {
            end += 1;
        }
        let open = self.is_open(directory);
        let mut fresh = Vec::new();
        if open {
            collect(directory, depth + 1, &self.expanded, &mut fresh);
        }
        let removed = end - (at + 1);
        let inserted = fresh.len();
        // The row the keyboard is on, by path rather than by index: a file appearing above it must
        // not carry the selection to a different file, which is exactly what an index would do.
        let was = self.entries.get(self.selected).map(|row| row.path.clone());
        self.entries.splice(at + 1..end, fresh);
        self.entries[at].expanded = open;
        self.selected = match was
            .as_ref()
            .and_then(|path| self.entries.iter().position(|row| row.path == *path))
        {
            // Still on screen: it is where it is now, wherever that turned out to be.
            Some(found) => found,
            // The row it was on is gone. The selection goes to the folder that was re-read, and
            // rows that only moved keep it where it was.
            None if self.selected >= end => self.selected - removed + inserted,
            None if self.selected > at => at,
            None => self.selected,
        };
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    /// Re-read whichever layer shows `path`, so a row that appeared or went away does too.
    ///
    /// The folder that holds the path is the only one that can have changed rows because of it. If
    /// that folder is not on screen, this is a no-op — and reads nothing.
    fn touch(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            if parent.starts_with(&self.root) {
                self.splice(parent);
            }
        }
    }

    /// `→`/`Enter`: open the folder under the selection.
    ///
    /// On a file this does nothing and still says it handled the key. Opening a file is the editor's
    /// job, and the tree has no second thing for `→` to mean: returning `Ignored` would let the
    /// app's own meaning for that key run instead, so one key would do two unrelated things.
    fn open_row(&mut self) {
        let Some(node) = self.entries.get(self.selected) else {
            return;
        };
        if node.kind != NodeKind::Directory || node.expanded {
            return;
        }
        let path = node.path.clone();
        self.set_expanded(&path, true);
    }

    /// `←`: fold the open folder under the selection, or walk out to the row it lives in.
    fn up(&mut self) {
        let Some(node) = self.entries.get(self.selected) else {
            return;
        };
        // The root is never folded — a tree with nothing in it is not a smaller tree — so the root
        // row falls through to the walk out, which finds no parent row and does nothing.
        if node.kind == NodeKind::Directory && node.expanded && node.depth > 0 {
            let path = node.path.clone();
            self.set_expanded(&path, false);
            return;
        }
        // A file has no contents to fold, so `←` means what it means on a folded folder: go to the
        // row this one is under.
        let Some(parent) = node.path.parent().map(Path::to_path_buf) else {
            return;
        };
        if let Some(at) = self.entries.iter().position(|row| row.path == parent) {
            self.selected = at;
        }
    }

    /// Move the selection, staying inside the list. The window follows at the next draw.
    fn move_selection(&mut self, step: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as isize - 1;
        self.selected = (self.selected as isize + step).clamp(0, last) as usize;
    }
}

impl Pane for FileTree {
    fn kind(&self) -> PaneKind {
        PaneKind::FileTree
    }

    fn title(&self) -> &str {
        "files"
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let styles = theme.styles();
        let height = area.height as usize;
        // The window is reconciled here because this is the only place that knows how many rows fit;
        // a key moves the selection and nothing else, and the window follows on the next frame.
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        if self.selected < self.top {
            self.top = self.selected;
        }
        if self.selected >= self.top + height {
            self.top = self.selected + 1 - height;
        }
        self.top = self.top.min(self.entries.len().saturating_sub(height));

        for (offset, node) in self.entries.iter().skip(self.top).take(height).enumerate() {
            let y = area.y + offset as u16;
            let selected = self.top + offset == self.selected;
            // A file something just wrote is drawn in the warning colour with a dot after its name:
            // the eye finds it without reading forty rows.
            let recent = node.kind != NodeKind::Note && self.recent.contains_key(&node.path);
            let base = match node.kind {
                NodeKind::Directory => styles.accent,
                NodeKind::File => styles.text,
                NodeKind::Note => styles.dim,
            };
            let mut style = if recent { styles.warn } else { base };
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
                // The whole row, not just the name: a highlight that stops at the end of a short
                // name is one the eye has to hunt for.
                buf.set_style(Rect::new(area.x, y, area.width, 1), style);
            }
            let prefix = match node.kind {
                // The theme's own chevron for an open folder, its ellipsis for a folded one: what is
                // missing from a folded folder is its contents.
                NodeKind::Directory if node.expanded => theme.glyphs.prompt,
                NodeKind::Directory => theme.glyphs.folded,
                NodeKind::File | NodeKind::Note => "  ",
            };
            let indent = (node.depth as u16).saturating_mul(INDENT);
            let room = area.width.saturating_sub(indent);
            if room == 0 {
                continue;
            }
            let x = area.x + indent;
            let (mut name_x, _) = buf.set_stringn(x, y, prefix, room as usize, style);
            // Where the name starts is where the prefix ended — but never inside the first two
            // cells, so names line up even where a glyph is one cell wide.
            if name_x < x + INDENT {
                name_x = x + INDENT;
            }
            let room = (area.x + area.width).saturating_sub(name_x) as usize;
            let (after, _) = buf.set_stringn(name_x, y, &node.label, room, style);
            if recent {
                let dot = after + 1;
                let room = (area.x + area.width).saturating_sub(dot) as usize;
                if room > 0 {
                    buf.set_stringn(dot, y, theme.glyphs.note, room, style);
                }
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> KeyOutcome {
        // A held modifier is somebody else's key: `Ctrl+R` is not this pane's `r`.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return KeyOutcome::Ignored;
        }
        match key.code {
            KeyCode::Up => {
                self.move_selection(-1);
                KeyOutcome::Handled
            }
            KeyCode::Down => {
                self.move_selection(1);
                KeyOutcome::Handled
            }
            KeyCode::Right | KeyCode::Enter => {
                self.open_row();
                KeyOutcome::Handled
            }
            KeyCode::Left => {
                self.up();
                KeyOutcome::Handled
            }
            KeyCode::Char('r') => {
                self.refresh();
                KeyOutcome::Handled
            }
            // Everything else belongs to whoever else wants it: quitting, focus, the shell.
            _ => KeyOutcome::Ignored,
        }
    }

    fn tick(&mut self) {
        self.tick += 1;
        // The mark ages out by counting, not by asking the clock: a pane that read the time would
        // answer differently in a test than in the app, which is the one thing a test cannot cover.
        let now = self.tick;
        self.recent
            .retain(|_, at| now.saturating_sub(*at) <= RECENT_TICKS);
    }
    fn wants_frame(&self) -> bool {
        // A recent mark fades over ticks, so while one is lit the frame it is in has to keep being
        // drawn: otherwise it would stay lit until something else happened to redraw.
        !self.recent.is_empty()
    }

    /// The wheel moves the selection, which is how this pane scrolls: a tree whose cursor is its
    /// selection has one answer to "what happens next", and a second scrolling offset would be a
    /// second answer.
    fn on_scroll(&mut self, steps: isize, _height: u16) {
        for _ in 0..steps.unsigned_abs() {
            self.move_selection(if steps > 0 { -1 } else { 1 });
        }
    }

    fn on_file_event(&mut self, event: &FileEvent) {
        let (path, removed) = match event {
            FileEvent::Changed { path } | FileEvent::EditorWrote { path } => {
                (path.as_path(), false)
            }
            FileEvent::Created { path } => (path.as_path(), false),
            FileEvent::Removed { path } => (path.as_path(), true),
        };
        // Defensive rather than necessary: the watcher publishes only paths under the session root,
        // but a pane whose behaviour is right only because the code that feeds it is right is a pane
        // that breaks when the feeding changes. A path outside the root is another project's business.
        if !path.starts_with(&self.root) {
            return;
        }
        if removed {
            self.recent.remove(path);
            // A folder that is gone is not open, and neither is anything that was inside it: keeping
            // the set would fold the wrong rows open if a path with the same name came back.
            self.expanded.retain(|open| !open.starts_with(path));
            self.touch(path);
            return;
        }
        // The app's own write is a change like any other: it is the file that was just touched.
        self.recent.insert(path.to_path_buf(), self.tick);
        // A `Created` may be a name no row has shown yet, so the folder it landed in is re-read. A
        // `Changed` can only be a row that is already there.
        if matches!(event, FileEvent::Created { .. }) {
            self.touch(path);
        }
    }
}

/// One folder's own layer, then — for the folders in it that are open — theirs, and so on.
///
/// The only place in this pane that reads the disk, and it reads a folder because that folder is on
/// screen: a folded folder is a row and nothing more. That is what keeps a deep project from costing
/// a walk over every file in it, and what makes a change under a folded folder cost nothing at all.
fn collect(directory: &Path, depth: usize, expanded: &BTreeSet<PathBuf>, rows: &mut Vec<Node>) {
    let reading = match std::fs::read_dir(directory) {
        Ok(reading) => reading,
        Err(error) => {
            rows.push(Node::note(
                directory,
                depth,
                format!("(cannot read: {error})"),
            ));
            return;
        }
    };
    let mut children: Vec<(bool, String, PathBuf)> = Vec::new();
    for entry in reading.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
        children.push((is_directory, name, entry.path()));
    }
    // Folders first, then by name. The name ordering is what makes two refreshes of one folder agree
    // with each other — `read_dir` promises nothing about order, and a tree that reshuffles under the
    // keyboard is a tree you cannot select anything in.
    children.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    if children.is_empty() {
        rows.push(Node::note(directory, depth, "(empty)".to_string()));
        return;
    }
    for (is_directory, label, path) in children {
        let open = is_directory && expanded.contains(&path);
        rows.push(Node {
            path: path.clone(),
            label,
            depth,
            kind: if is_directory {
                NodeKind::Directory
            } else {
                NodeKind::File
            },
            expanded: open,
        });
        if open {
            collect(&path, depth + 1, expanded, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-files-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The usual shape: two folders, two files, and something inside one of the folders.
    fn project(name: &str) -> PathBuf {
        let dir = scratch(name);
        std::fs::create_dir_all(dir.join("alpha/nested")).unwrap();
        std::fs::create_dir_all(dir.join("beta")).unwrap();
        std::fs::write(dir.join("alpha/a.txt"), "a").unwrap();
        std::fs::write(dir.join("one.txt"), "1").unwrap();
        std::fs::write(dir.join("two.txt"), "2").unwrap();
        dir
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn area() -> Rect {
        Rect::new(0, 0, 32, 8)
    }

    /// What the pane draws, one line per row, trailing blanks removed.
    fn rows(tree: &mut FileTree) -> Vec<String> {
        let mut buf = Buffer::empty(area());
        tree.draw(area(), &mut buf, &Theme::default());
        (0..area().height)
            .map(|row| {
                (0..area().width)
                    .map(|column| buf[(column, row)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The rows that have something on them.
    fn lines(tree: &mut FileTree) -> Vec<String> {
        rows(tree)
            .into_iter()
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// Whether any drawn row mentions `text`.
    fn seen(tree: &mut FileTree, text: &str) -> bool {
        lines(tree).iter().any(|line| line.contains(text))
    }

    /// The name of the row the keyboard is on.
    fn selected(tree: &FileTree) -> String {
        tree.entries[tree.selected].label.clone()
    }

    /// The theme's own mark for a row the app is saying something about, which is what a recent
    /// change is drawn with. Taken from the theme so a change of glyph fails one place, not ten.
    fn mark() -> &'static str {
        Theme::default().glyphs.note.trim_end()
    }

    #[test]
    fn the_first_draw_lists_the_root_layer_only_directories_first() {
        let dir = project("first");
        let mut tree = FileTree::new(&dir);
        let drawn = lines(&mut tree);

        let root = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            drawn.len(),
            5,
            "根这一层：根 + 两个目录 + 两个文件: {drawn:?}"
        );
        assert!(drawn[0].contains(&root), "第一行是根: {drawn:?}");
        assert!(drawn[1].ends_with("alpha"), "{drawn:?}");
        assert!(drawn[2].ends_with("beta"), "目录排在文件前面: {drawn:?}");
        assert!(drawn[3].ends_with("one.txt"), "{drawn:?}");
        assert!(drawn[4].ends_with("two.txt"), "{drawn:?}");
        assert!(
            !drawn
                .iter()
                .any(|line| line.contains("a.txt") || line.contains("nested")),
            "没有展开的目录不读盘，更不该出现在屏幕上: {drawn:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_a_folder_puts_its_children_under_it_and_closing_takes_them_away() {
        let glyphs = Theme::default().glyphs;
        let dir = project("open");
        let mut tree = FileTree::new(&dir);

        tree.on_key(key(KeyCode::Down)); // on `alpha`
        assert_eq!(selected(&tree), "alpha");
        assert_eq!(tree.on_key(key(KeyCode::Right)), KeyOutcome::Handled);
        let drawn = lines(&mut tree);
        assert_eq!(drawn.len(), 7, "alpha 的两个子项接在它后面: {drawn:?}");
        assert!(
            drawn[1].trim_start().starts_with(glyphs.prompt),
            "展开的目录用展开的字形: {drawn:?}"
        );
        assert!(
            drawn[2].ends_with("nested"),
            "子目录在正确的位置: {drawn:?}"
        );
        assert!(drawn[3].ends_with("a.txt"), "子文件在正确的位置: {drawn:?}");
        assert!(drawn[4].ends_with("beta"), "别的行没有动: {drawn:?}");

        assert_eq!(tree.on_key(key(KeyCode::Left)), KeyOutcome::Handled);
        let after = lines(&mut tree);
        assert_eq!(after.len(), 5, "收起后子项消失: {after:?}");
        assert!(after[1].ends_with("alpha"), "{after:?}");
        assert!(
            after[1].trim_start().starts_with(glyphs.folded),
            "收起的目录用收起的字形: {after:?}"
        );
        assert!(
            !after.iter().any(|line| line.contains("a.txt")),
            "{after:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_change_marks_its_row_and_the_mark_fades_on_its_own() {
        let dir = project("recent");
        let mut tree = FileTree::new(&dir);
        assert!(
            !seen(&mut tree, mark()),
            "什么都没发生的时候没有标记: {:?}",
            lines(&mut tree)
        );

        tree.on_file_event(&FileEvent::Changed {
            path: dir.join("one.txt"),
        });
        let drawn = lines(&mut tree);
        let marked: Vec<&String> = drawn.iter().filter(|line| line.contains(mark())).collect();
        assert_eq!(marked.len(), 1, "只有改动过的那一行有标记: {drawn:?}");
        assert!(marked[0].contains("one.txt"), "而且就是那一行: {drawn:?}");

        // And it is drawn in the colour that means "just touched", not in the colour of a file
        // nobody has touched: the mark is a mark, and a mark nobody can tell from the rest of the
        // list is not one.
        let theme = Theme::default();
        let row = drawn.iter().position(|line| line.contains(mark())).unwrap() as u16;
        let mut buf = Buffer::empty(area());
        tree.draw(area(), &mut buf, &theme);
        assert_eq!(buf[(4, row)].symbol(), "o", "缩进之后就是文件名: {drawn:?}");
        assert_eq!(buf[(4, row)].fg, theme.palette.warn, "最近改动用 warn 色");
        let plain = drawn
            .iter()
            .position(|line| line.contains("two.txt"))
            .unwrap() as u16;
        assert_eq!(buf[(4, plain)].fg, theme.palette.text, "没动过的还是正文色");

        for _ in 0..RECENT_TICKS {
            tree.tick();
        }
        assert!(
            seen(&mut tree, mark()),
            "还没到时间，标记还在（{} 个 tick）",
            RECENT_TICKS
        );
        tree.tick(); // RECENT_TICKS + 1
        assert!(
            !seen(&mut tree, mark()),
            "超过 {} 个 tick 之后标记自己褪掉: {:?}",
            RECENT_TICKS,
            lines(&mut tree)
        );
        assert!(seen(&mut tree, "one.txt"), "褪掉的只是标记，不是那一行");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_apps_own_write_is_a_change_too() {
        let dir = project("editor-wrote");
        let mut tree = FileTree::new(&dir);
        tree.on_file_event(&FileEvent::EditorWrote {
            path: dir.join("two.txt"),
        });
        let drawn = lines(&mut tree);
        let marked: Vec<&String> = drawn.iter().filter(|line| line.contains(mark())).collect();
        assert_eq!(marked.len(), 1, "自己写的那一份也算刚动过: {drawn:?}");
        assert!(marked[0].contains("two.txt"), "{drawn:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removed_file_leaves_the_folder_it_was_in() {
        let dir = project("removed");
        let mut tree = FileTree::new(&dir);
        assert!(seen(&mut tree, "two.txt"));

        std::fs::remove_file(dir.join("two.txt")).unwrap();
        tree.on_file_event(&FileEvent::Removed {
            path: dir.join("two.txt"),
        });
        let drawn = lines(&mut tree);
        assert!(
            !drawn.iter().any(|line| line.contains("two.txt")),
            "删掉的行从展开的目录里消失: {drawn:?}"
        );
        assert_eq!(drawn.len(), 4, "只少了那一行: {drawn:?}");
        assert!(
            drawn.iter().any(|line| line.ends_with("one.txt")),
            "同一层的其它行还在: {drawn:?}"
        );
        assert!(!seen(&mut tree, mark()), "已经不在的东西不是「刚改过」");

        // A folder that is not open is not re-read: a change inside one is not a row, so nothing
        // about it can move on screen.
        std::fs::remove_file(dir.join("alpha/a.txt")).unwrap();
        tree.on_file_event(&FileEvent::Removed {
            path: dir.join("alpha/a.txt"),
        });
        assert_eq!(lines(&mut tree), drawn, "收起的目录里的事不改变屏幕");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_outside_the_root_changes_nothing() {
        let dir = project("outside");
        let mut tree = FileTree::new(&dir);
        let before = lines(&mut tree);
        let elsewhere = std::env::temp_dir().join(format!(
            "jmds-files-{}-outside-elsewhere.txt",
            std::process::id()
        ));

        // `Removed` first and `Changed` last, so the invariant below is read after the only events
        // that can leave something behind.
        tree.on_file_event(&FileEvent::Removed {
            path: elsewhere.clone(),
        });
        tree.on_file_event(&FileEvent::Created {
            path: elsewhere.clone(),
        });
        tree.on_file_event(&FileEvent::EditorWrote {
            path: elsewhere.clone(),
        });
        tree.on_file_event(&FileEvent::Changed { path: elsewhere });

        assert_eq!(lines(&mut tree), before, "根之外的行一行都没有变");
        assert!(!seen(&mut tree, mark()), "根之外的改动不该有标记");
        assert!(
            tree.recent.is_empty(),
            "根之外的路径连记都不该记：这个 pane 的状态只属于它自己的那棵树"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walking_past_the_bottom_scrolls_the_window_with_the_selection() {
        let dir = scratch("scroll");
        for index in 0..12 {
            std::fs::write(dir.join(format!("f{index:02}.txt")), "x").unwrap();
        }
        let mut tree = FileTree::new(&dir);
        let root = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(selected(&tree), root, "一开始选中根");

        for _ in 0..12 {
            tree.on_key(key(KeyCode::Down));
        }
        let selected = selected(&tree);
        assert_eq!(selected, "f11.txt", "到底了就不动了");
        let drawn = lines(&mut tree);
        assert_eq!(drawn.len(), area().height as usize, "整屏都是行: {drawn:?}");
        assert!(
            drawn.iter().any(|line| line.contains(&selected)),
            "选中的那一行仍然看得见: {drawn:?}"
        );
        assert!(
            !drawn.iter().any(|line| line.contains(&root)),
            "窗口跟着滚下去了: {drawn:?}"
        );

        // The highlight covers the row, not just the name: a bar that stops at the end of a short
        // name is one the eye has to hunt for.
        let row = drawn
            .iter()
            .position(|line| line.contains(&selected))
            .unwrap() as u16;
        let mut buf = Buffer::empty(area());
        tree.draw(area(), &mut buf, &Theme::default());
        assert!(
            buf[(0, row)].modifier.contains(Modifier::REVERSED),
            "选中行反显: {:?}",
            buf[(0, row)]
        );
        assert!(
            !buf[(0, 0)].modifier.contains(Modifier::REVERSED),
            "别的行不反显"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_file_appears_in_the_layer_it_landed_in() {
        let dir = project("created");
        let mut tree = FileTree::new(&dir);
        assert!(!seen(&mut tree, "three.txt"));

        // A write the watcher does announce: the row is there without anybody pressing anything.
        let path = dir.join("three.txt");
        std::fs::write(&path, "3").unwrap();
        tree.on_file_event(&FileEvent::Created { path });
        let drawn = lines(&mut tree);
        assert_eq!(drawn.len(), 6, "根这一层多了一行: {drawn:?}");
        let marked: Vec<&String> = drawn.iter().filter(|line| line.contains(mark())).collect();
        assert_eq!(marked.len(), 1, "{drawn:?}");
        assert!(
            marked[0].contains("three.txt"),
            "新的那一行就是刚改过的那一行: {drawn:?}"
        );

        // A new file inside a folder that is folded is not a row: nothing in that folder is. The
        // mark is still remembered, so opening it shows the file lit.
        let deep = dir.join("alpha/a2.txt");
        std::fs::write(&deep, "a").unwrap();
        tree.on_file_event(&FileEvent::Created { path: deep });
        assert_eq!(
            lines(&mut tree).len(),
            6,
            "收起的目录不会因为一次改动就展开"
        );
        tree.on_key(key(KeyCode::Down)); // `alpha`
        tree.on_key(key(KeyCode::Right));
        let drawn = lines(&mut tree);
        assert!(
            drawn
                .iter()
                .any(|line| line.contains("a2.txt") && line.contains(mark())),
            "展开以后它在，而且带着标记: {drawn:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lit_mark_keeps_asking_for_frames_until_it_fades() {
        let root = project("wants-frame");
        let mut tree = FileTree::new(&root);
        assert!(!tree.wants_frame(), "没变化就没有要动的东西");

        tree.on_file_event(&FileEvent::Changed {
            path: root.join("alpha.txt"),
        });
        assert!(tree.wants_frame(), "标记亮着的时候得继续画，否则它永远亮着");

        // The mark ages out by ticks, and once it has, the pane stops asking.
        for _ in 0..=RECENT_TICKS {
            tree.tick();
        }
        assert!(!tree.wants_frame());
    }

    #[test]
    fn r_rereads_the_layers_on_screen() {
        let dir = project("reread");
        let mut tree = FileTree::new(&dir);
        assert!(seen(&mut tree, "two.txt"));

        // Something changed on disk that no event announced — a file removed by a script that is
        // not a tool call: `r` is how a person asks the tree to look again.
        std::fs::remove_file(dir.join("two.txt")).unwrap();
        assert!(seen(&mut tree, "two.txt"), "没人说，它当然还画着老的那一份");
        assert_eq!(tree.on_key(key(KeyCode::Char('r'))), KeyOutcome::Handled);
        let drawn = lines(&mut tree);
        assert!(
            !drawn.iter().any(|line| line.contains("two.txt")),
            "r 重扫屏幕上那几层: {drawn:?}"
        );
        assert!(
            drawn.iter().any(|line| line.ends_with("one.txt")),
            "{drawn:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_list_that_shrinks_pulls_the_window_back_inside_it() {
        let dir = scratch("shrink");
        for index in 0..12 {
            std::fs::write(dir.join(format!("f{index:02}.txt")), "x").unwrap();
        }
        let mut tree = FileTree::new(&dir);
        let root = dir.file_name().unwrap().to_string_lossy().into_owned();
        for _ in 0..12 {
            tree.on_key(key(KeyCode::Down));
        }
        assert!(!seen(&mut tree, &root), "窗口已经滚到下面了");

        // Six of the files go away: there is now less to show than the top of the window assumed,
        // and a window left where it was would draw two rows in a pane with room for eight.
        for index in 0..6 {
            let path = dir.join(format!("f{index:02}.txt"));
            std::fs::remove_file(&path).unwrap();
            tree.on_file_event(&FileEvent::Removed { path });
        }
        let drawn = lines(&mut tree);
        assert_eq!(drawn.len(), 7, "有七行就画七行，不留空白: {drawn:?}");
        assert!(drawn[0].contains(&root), "窗口被拉回列表里面: {drawn:?}");
        assert_eq!(selected(&tree), "f11.txt", "选中的那一行没有被换掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_cannot_be_read_says_so_instead_of_drawing_nothing() {
        let dir = scratch("unreadable");
        let file = dir.join("not-a-directory");
        std::fs::write(&file, "x").unwrap();
        // The root is a file: reading it as a folder fails, which is the one way a tree is broken
        // without a permission bit that a test process might not be able to set.
        let mut tree = FileTree::new(&file);
        let drawn = lines(&mut tree);
        assert!(drawn[0].contains("not-a-directory"), "{drawn:?}");
        assert!(
            drawn[1].contains("cannot read"),
            "给了说明而不是空白: {drawn:?}"
        );

        // A broken tree is still a tree: the keys answer.
        assert_eq!(tree.on_key(key(KeyCode::Down)), KeyOutcome::Handled);
        assert_eq!(tree.on_key(key(KeyCode::Right)), KeyOutcome::Handled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_folder_says_it_is_empty() {
        let dir = project("empty");
        std::fs::create_dir_all(dir.join("hollow")).unwrap();
        let mut tree = FileTree::new(&dir);

        tree.on_key(key(KeyCode::Down));
        tree.on_key(key(KeyCode::Down));
        tree.on_key(key(KeyCode::Down));
        assert_eq!(selected(&tree), "hollow");
        tree.on_key(key(KeyCode::Right));
        let drawn = lines(&mut tree);
        assert!(
            drawn.iter().any(|line| line.contains("(empty)")),
            "空目录有一行说明，不是一片空白: {drawn:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn re_rooting_forgets_the_old_tree_and_staying_put_does_not() {
        let dir = project("reroot");
        let other = scratch("reroot-other");
        std::fs::create_dir_all(other.join("gamma")).unwrap();
        std::fs::write(other.join("gamma/g.txt"), "g").unwrap();

        let mut tree = FileTree::new(&dir);
        tree.on_key(key(KeyCode::Down));
        tree.on_key(key(KeyCode::Right));
        tree.on_file_event(&FileEvent::Changed {
            path: dir.join("one.txt"),
        });
        assert!(seen(&mut tree, "a.txt"), "alpha 是展开的");
        assert!(seen(&mut tree, mark()));

        // Where it already is: telling it again must not close what a person opened, because a host
        // may hand over the working directory every frame.
        tree.set_root(&dir);
        assert!(seen(&mut tree, "a.txt"), "同一个根不重置展开态");
        assert!(seen(&mut tree, mark()), "同一个根不丢掉标记");

        tree.set_root(&other);
        assert_eq!(tree.root(), other.as_path());
        let drawn = lines(&mut tree);
        assert!(
            drawn.iter().any(|line| line.contains("gamma")),
            "换上新的根: {drawn:?}"
        );
        assert!(
            !drawn
                .iter()
                .any(|line| line.contains("a.txt") || line.contains("one.txt")),
            "上一棵树的行不在了: {drawn:?}"
        );
        assert!(
            !seen(&mut tree, mark()),
            "换个项目以后，上一个项目刚改过的路径没有意义"
        );
        assert_eq!(
            selected(&tree),
            other.file_name().unwrap().to_string_lossy()
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }
}
