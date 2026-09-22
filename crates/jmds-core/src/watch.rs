//! Watching the project: which changes matter, and saying so once.
//!
//! Two halves, deliberately apart.
//!
//! [`Ignore`] and [`Batch`] are pure: given a stream of raw notifications they decide what is worth
//! publishing, and none of that needs a file system — which is what makes the rules testable
//! without waiting for a real editor to save a real file. [`Watcher`] is the thin half: notify's OS
//! notifications go into a channel, one thread takes them out in windows, and what survives is
//! published on the bus.
//!
//! Why a window at all: a save is not one system call. Editors write a temporary file, rename it
//! over the target, and truncate something in between, and every one of those steps is a
//! notification. Publishing them one by one would make a pane that reloads on change reload three
//! times per save.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::broadcast::error::TryRecvError;

use crate::event::{Event, EventBus, FileEvent};

/// How long a burst is collected before it is published.
///
/// Long enough that the several notifications of one save land in the same window, short enough
/// that a pane reacting to a change still feels immediate.
pub const WINDOW: Duration = Duration::from_millis(120);

/// What happened to one path, after notify's vocabulary has been narrowed to what this app acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Created,
    Changed,
    Removed,
}

impl Change {
    /// How a second notification for the same path folds into the first one, if at all.
    ///
    /// The rule is that the more informative of the two survives, not simply the later one. A file
    /// created and then written was created: a listing needs to know to look again, and "it changed"
    /// would leave a new file invisible until something else happened to refresh the list.
    fn merge(self, next: Self) -> Option<Self> {
        use Change::*;
        match (self, next) {
            // Deleted and recreated is one file saved again: an editor that writes a temporary file
            // and renames it over the target looks exactly like this, and reporting it as a new
            // file would make every save look like a new file.
            (Removed, Created) => Some(Changed),
            // Created and gone again inside one window is a temporary file that came and went.
            // Nobody needs to hear about it.
            (Created, Removed) => None,
            // New, and then written to, in the same breath.
            (Created, Changed) => Some(Created),
            (_, next) => Some(next),
        }
    }

    fn into_event(self, path: PathBuf) -> FileEvent {
        match self {
            Change::Created => FileEvent::Created { path },
            Change::Changed => FileEvent::Changed { path },
            Change::Removed => FileEvent::Removed { path },
        }
    }
}

/// What one notify event means, as a list of paths and what happened to each.
///
/// A rename is why this returns a list: notify reports one event with two paths (or two events with
/// one each, depending on the backend), and the two ends mean opposite things — the old path is
/// gone, the new one is new.
pub fn classify(kind: &EventKind, paths: &[PathBuf]) -> Vec<(PathBuf, Change)> {
    let one = |change: Change| paths.iter().cloned().map(|path| (path, change)).collect();
    match kind {
        // A read is not a change: a pane reloading because someone looked at a file would be a pane
        // reloading constantly.
        EventKind::Access(_) => Vec::new(),
        EventKind::Create(_) => one(Change::Created),
        EventKind::Remove(_) => one(Change::Removed),
        EventKind::Modify(ModifyKind::Name(mode)) => match (mode, paths) {
            (RenameMode::From, _) => one(Change::Removed),
            (RenameMode::To, _) => one(Change::Created),
            // The rename arrived as a single event carrying both ends, in that order.
            (RenameMode::Both, [from, to, ..]) => vec![
                (from.clone(), Change::Removed),
                (to.clone(), Change::Created),
            ],
            // Both ends were reported but the paths cannot be told apart. A change is the honest
            // answer: something about this path is different, and saying which way would be a guess.
            _ => one(Change::Changed),
        },
        EventKind::Modify(_) => one(Change::Changed),
        // `Any` and `Other`: notify could not say. A path that produced a notification did change,
        // and dropping it would mean a pane silently missing updates.
        _ => one(Change::Changed),
    }
}

/// Which paths a watcher does not care about.
///
/// Patterns are matched against the path relative to the watched root, so one rule holds wherever
/// the project sits.
#[derive(Debug, Clone)]
pub struct Ignore {
    set: GlobSet,
}

/// What is ignored unless someone says otherwise: what a build leaves behind and what version
/// control keeps. Watching those costs a notification per compile and tells nobody anything.
pub const DEFAULT_PATTERNS: &[&str] = &[
    "**/.git/**",
    "**/target/**",
    "**/node_modules/**",
    "**/.venv/**",
    "**/__pycache__/**",
    // Editor leftovers: swap files, backups, and the numbered file vim leaves mid-write.
    "**/*.swp",
    "**/*.swx",
    "**/*~",
    "**/*.tmp",
    "**/.DS_Store",
    "**/4913",
];

impl Default for Ignore {
    fn default() -> Self {
        Self::from_patterns(DEFAULT_PATTERNS)
    }
}

impl Ignore {
    /// Build a set from glob patterns. A pattern that does not compile is dropped with a warning:
    /// one bad rule in a config file should not stop the app from watching anything at all.
    pub fn from_patterns(patterns: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            match Glob::new(pattern.as_ref()) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(error) => log::warn!("忽略规则看不懂，已跳过：{}（{error}）", pattern.as_ref()),
            }
        }
        Self {
            set: builder.build().unwrap_or_else(|_| GlobSet::empty()),
        }
    }

    /// Whether this path is one nobody asked about.
    ///
    /// A path outside the root is ignored: the watcher was pointed at a project, and a symlink that
    /// leads elsewhere is not part of it.
    pub fn ignores(&self, root: &Path, path: &Path) -> bool {
        match path.strip_prefix(root) {
            Ok(relative) => relative.as_os_str().is_empty() || self.set.is_match(relative),
            Err(_) => true,
        }
    }
}

/// The paths this app wrote itself.
///
/// The write side announces its own writes before making them ([`FileEvent::EditorWrote`]), and the
/// watcher remembers the announcement so the notifications that follow are not published back. The
/// memory lasts exactly one window, which is the granularity the question is asked at: one write
/// produces several notifications (create, modify, modify), they all belong to the window the
/// announcement preceded, and anything a later window reports is somebody else's.
#[derive(Debug, Default)]
pub struct Ours {
    waiting: HashSet<PathBuf>,
}

impl Ours {
    /// Record an announcement.
    pub fn announced(&mut self, path: impl Into<PathBuf>) {
        if self.waiting.len() >= 512 {
            self.waiting.clear();
        }
        self.waiting.insert(path.into());
    }

    /// Whether this path's notification is one of ours.
    pub fn takes(&self, path: &Path) -> bool {
        self.waiting.contains(path)
    }

    /// Start a new window: what was ours a moment ago is nobody's to claim now.
    pub fn forget(&mut self) {
        self.waiting.clear();
    }
}

/// Changes collected inside the current window.
#[derive(Debug, Default)]
pub struct Batch {
    /// First-seen order, so the published order does not depend on hash iteration.
    order: Vec<PathBuf>,
    kinds: HashMap<PathBuf, Change>,
}

impl Batch {
    /// Fold one notification in, applying [`Change::merge`]. A merge that yields nothing drops the
    /// path entirely.
    pub fn absorb(&mut self, path: PathBuf, change: Change) {
        match self
            .kinds
            .get(&path)
            .copied()
            .map(|current| current.merge(change))
        {
            Some(Some(merged)) => {
                self.kinds.insert(path, merged);
            }
            Some(None) => {
                self.kinds.remove(&path);
                self.order.retain(|held| held != &path);
            }
            None => {
                self.order.push(path.clone());
                self.kinds.insert(path, change);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Take everything collected, in the order the paths were first seen.
    pub fn drain(&mut self) -> Vec<FileEvent> {
        let events = self
            .order
            .drain(..)
            .filter_map(|path| {
                self.kinds
                    .remove(&path)
                    .map(|change| change.into_event(path))
            })
            .collect();
        self.kinds.clear();
        events
    }
}

/// A running watcher.
///
/// Dropping it stops the watch: notify's watcher is what holds the OS-level subscription, and the
/// thread ends when its channel closes.
pub struct Watcher {
    /// Kept alive on purpose: it is what holds the OS subscription. `Option` so `Drop` can let go of
    /// it before waiting for the thread, which is what makes the thread end.
    inner: Option<RecommendedWatcher>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    /// Watch `root` recursively, publishing what matters to `bus`.
    pub fn watch(root: impl AsRef<Path>, bus: EventBus) -> notify::Result<Self> {
        Self::watch_ignoring(root, bus, Ignore::default())
    }

    /// Watch `root` with a caller's ignore rules — what a config file would supply.
    pub fn watch_ignoring(
        root: impl AsRef<Path>,
        bus: EventBus,
        ignore: Ignore,
    ) -> notify::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let (tx, rx): (Sender<NotifyEvent>, Receiver<NotifyEvent>) = std::sync::mpsc::channel();
        let mut inner = notify::recommended_watcher(move |event: notify::Result<NotifyEvent>| {
            // A backend error is not the end of the watch: the next event still arrives.
            match event {
                Ok(event) => {
                    let _ = tx.send(event);
                }
                Err(error) => log::warn!("文件监视出错：{error}"),
            }
        })?;
        inner.watch(&root, RecursiveMode::Recursive)?;

        let mut announcements = bus.subscribe();
        let worker = std::thread::spawn(move || {
            let mut ours = Ours::default();
            let mut batch = Batch::default();
            loop {
                match rx.recv_timeout(WINDOW) {
                    Ok(event) => {
                        // What the app said about itself is read here, after the notification
                        // arrived: the announcement is published before the write, so by the time
                        // the write's notification comes back it is already waiting on the bus.
                        // Reading it before the wait would race with the loop's first iteration.
                        drain_announcements(&mut announcements, &mut ours, &root);
                        for (path, change) in classify(&event.kind, &event.paths) {
                            if ignore.ignores(&root, &path) || ours.takes(&path) {
                                continue;
                            }
                            batch.absorb(path, change);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => publish(&mut batch, &mut ours, &bus),
                    Err(RecvTimeoutError::Disconnected) => {
                        drain_announcements(&mut announcements, &mut ours, &root);
                        publish(&mut batch, &mut ours, &bus);
                        break;
                    }
                }
            }
        });

        Ok(Self {
            inner: Some(inner),
            worker: Some(worker),
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Letting go of the notify watcher closes the sending end of the channel, which ends the
        // loop; the join is so the thread is not still publishing into a bus the app has left.
        drop(self.inner.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Take any `EditorWrote` off the bus and remember the path.
fn drain_announcements(
    announcements: &mut tokio::sync::broadcast::Receiver<Event>,
    ours: &mut Ours,
    root: &Path,
) {
    loop {
        match announcements.try_recv() {
            Ok(Event::File(FileEvent::EditorWrote { path })) => {
                if path.starts_with(root) {
                    ours.announced(path);
                }
            }
            Ok(_) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => return,
            Err(TryRecvError::Lagged(missed)) => {
                // Falling behind means an announcement may have been missed, and a missed
                // announcement is one notification published twice. Saying so beats pretending.
                log::warn!("文件监视漏掉了 {missed} 条事件");
            }
        }
    }
}

fn publish(batch: &mut Batch, ours: &mut Ours, bus: &EventBus) {
    for event in batch.drain() {
        bus.publish(event);
    }
    // The window is over: a notification that arrives next belongs to a change nobody announced.
    ours.forget();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    fn file_path(event: &FileEvent) -> String {
        match event {
            FileEvent::Created { path }
            | FileEvent::Changed { path }
            | FileEvent::Removed { path }
            | FileEvent::EditorWrote { path } => path.display().to_string(),
        }
    }

    #[test]
    fn what_the_filesystem_says_is_narrowed_to_three_things() {
        assert_eq!(
            classify(
                &EventKind::Create(notify::event::CreateKind::File),
                &paths(&["a.txt"])
            ),
            vec![(PathBuf::from("a.txt"), Change::Created)]
        );
        assert_eq!(
            classify(
                &EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                &paths(&["a.txt"])
            ),
            vec![(PathBuf::from("a.txt"), Change::Changed)]
        );
        assert_eq!(
            classify(
                &EventKind::Remove(notify::event::RemoveKind::File),
                &paths(&["a.txt"])
            ),
            vec![(PathBuf::from("a.txt"), Change::Removed)]
        );
        assert!(
            classify(
                &EventKind::Access(notify::event::AccessKind::Any),
                &paths(&["a.txt"])
            )
            .is_empty(),
            "读不是改动"
        );
    }

    #[test]
    fn a_rename_is_a_removal_and_a_creation() {
        assert_eq!(
            classify(
                &EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &paths(&["old.rs", "new.rs"]),
            ),
            vec![
                (PathBuf::from("old.rs"), Change::Removed),
                (PathBuf::from("new.rs"), Change::Created),
            ]
        );
        // A backend that reports the two ends as two events.
        assert_eq!(
            classify(
                &EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &paths(&["old.rs"])
            ),
            vec![(PathBuf::from("old.rs"), Change::Removed)]
        );
        assert_eq!(
            classify(
                &EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &paths(&["new.rs"])
            ),
            vec![(PathBuf::from("new.rs"), Change::Created)]
        );
    }

    #[test]
    fn a_save_is_one_change_not_five() {
        let mut batch = Batch::default();
        batch.absorb(PathBuf::from("src/main.rs"), Change::Changed);
        batch.absorb(PathBuf::from("src/main.rs"), Change::Changed);
        batch.absorb(PathBuf::from("src/main.rs"), Change::Changed);
        assert_eq!(
            batch.drain(),
            vec![FileEvent::Changed {
                path: PathBuf::from("src/main.rs")
            }]
        );
        assert!(batch.is_empty(), "抽干了就是抽干了");
    }

    #[test]
    fn a_file_created_and_then_written_is_still_a_creation() {
        // The two notifications every new file produces. Reporting it as merely "changed" would
        // leave it invisible in anything that lists a directory by looking only when it hears of a
        // new entry — which is exactly what a file tree does.
        let mut batch = Batch::default();
        batch.absorb(PathBuf::from("delta.txt"), Change::Created);
        batch.absorb(PathBuf::from("delta.txt"), Change::Changed);
        assert_eq!(
            batch.drain(),
            vec![FileEvent::Created {
                path: PathBuf::from("delta.txt")
            }]
        );
    }

    #[test]
    fn a_temporary_file_created_written_and_deleted_is_still_nothing() {
        let mut batch = Batch::default();
        batch.absorb(PathBuf::from(".a.txt.swp"), Change::Created);
        batch.absorb(PathBuf::from(".a.txt.swp"), Change::Changed);
        batch.absorb(PathBuf::from(".a.txt.swp"), Change::Removed);
        assert_eq!(batch.drain(), Vec::new());
    }

    #[test]
    fn a_file_deleted_and_rewritten_within_the_window_is_a_change() {
        let mut batch = Batch::default();
        batch.absorb(PathBuf::from("a.txt"), Change::Removed);
        batch.absorb(PathBuf::from("a.txt"), Change::Created);
        assert_eq!(
            batch.drain(),
            vec![FileEvent::Changed {
                path: PathBuf::from("a.txt")
            }],
            "原子保存覆盖旧文件，不是新文件"
        );
    }

    #[test]
    fn a_temporary_file_that_comes_and_goes_is_not_reported() {
        let mut batch = Batch::default();
        batch.absorb(PathBuf::from(".a.txt.swp"), Change::Created);
        batch.absorb(PathBuf::from(".a.txt.swp"), Change::Removed);
        assert_eq!(batch.drain(), Vec::new());
    }

    #[test]
    fn the_order_is_the_order_things_were_first_seen() {
        let mut batch = Batch::default();
        batch.absorb(PathBuf::from("b.txt"), Change::Changed);
        batch.absorb(PathBuf::from("a.txt"), Change::Changed);
        batch.absorb(PathBuf::from("b.txt"), Change::Changed);
        let drained: Vec<String> = batch.drain().iter().map(file_path).collect();
        assert_eq!(drained, ["b.txt", "a.txt"]);
    }

    #[test]
    fn a_build_directory_and_a_swap_file_are_not_worth_watching() {
        let ignore = Ignore::default();
        let root = Path::new("/work");
        assert!(ignore.ignores(root, Path::new("/work/target/debug/jmds")));
        assert!(ignore.ignores(root, Path::new("/work/.git/index")));
        assert!(ignore.ignores(root, Path::new("/work/crates/x/node_modules/y.js")));
        assert!(ignore.ignores(root, Path::new("/work/src/main.rs.swp")));
        assert!(ignore.ignores(root, Path::new("/work/src/main.rs~")));
        assert!(
            ignore.ignores(root, Path::new("/elsewhere/outside.rs")),
            "不是这个项目的东西"
        );
        assert!(!ignore.ignores(root, Path::new("/work/src/main.rs")));
        assert!(
            !ignore.ignores(root, Path::new("/work/targets.rs")),
            "只是名字里有 target"
        );
    }

    #[test]
    fn an_announcement_covers_every_notification_in_its_window() {
        let mut ours = Ours::default();
        ours.announced("/work/a.txt");
        // One write produces several notifications, and they all belong to the announcement.
        assert!(ours.takes(Path::new("/work/a.txt")), "自己写的那条不回放");
        assert!(
            ours.takes(Path::new("/work/a.txt")),
            "同一次写入的第二条也不回放"
        );
        assert!(!ours.takes(Path::new("/work/b.txt")));

        // A new window: what was ours is nobody's to claim now.
        ours.forget();
        assert!(
            !ours.takes(Path::new("/work/a.txt")),
            "下一个窗口里的改动是别人的"
        );
    }

    #[test]
    fn a_config_pattern_that_does_not_compile_does_not_stop_the_others() {
        let ignore = Ignore::from_patterns(["**/*.log", "[这个不是 glob"]);
        let root = Path::new("/work");
        assert!(ignore.ignores(root, Path::new("/work/a.log")));
        assert!(!ignore.ignores(root, Path::new("/work/a.rs")));
    }

    /// A project of our own: a temporary directory, a bus, and a thread that moves events out of the
    /// broadcast channel so a test can wait for them with a deadline instead of forever.
    struct Fixture {
        dir: PathBuf,
        bus: EventBus,
        events: std::sync::mpsc::Receiver<Event>,
        watcher: Option<Watcher>,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("jmds-watch-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let bus = EventBus::new(64);
            let (tx, rx) = std::sync::mpsc::channel();
            let mut subscription = bus.subscribe();
            std::thread::spawn(move || {
                while let Ok(event) = subscription.blocking_recv() {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            });
            Self {
                dir,
                bus,
                events: rx,
                watcher: None,
            }
        }

        /// Start watching. Separate from `new` so a test can put files in place first: a directory
        /// created before the watch is not an event, and a fixture should not be one either.
        fn watch(&mut self) {
            self.watcher = Some(Watcher::watch(&self.dir, self.bus.clone()).expect("一个监视器"));
        }

        fn next(&self, within: Duration) -> Option<Event> {
            self.events.recv_timeout(within).ok()
        }

        /// Read file events until one lands on `name`, returning everything seen, that one included.
        ///
        /// `EditorWrote` is skipped: that is the write side announcing itself, not the watcher
        /// saying what changed, and the two are different questions.
        fn until(&self, name: &str, within: Duration) -> Vec<FileEvent> {
            let mut seen = Vec::new();
            while let Some(event) = self.next(within) {
                if let Event::File(file) = event {
                    if matches!(file, FileEvent::EditorWrote { .. }) {
                        continue;
                    }
                    let reached = file_path(&file).ends_with(name);
                    seen.push(file);
                    if reached {
                        break;
                    }
                }
            }
            seen
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.watcher.take();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_write_under_the_root_comes_back_as_one_change() {
        let mut fixture = Fixture::new("write");
        fixture.watch();
        std::fs::write(fixture.dir.join("a.txt"), "one").unwrap();

        match fixture.next(Duration::from_secs(5)).expect("一条通知") {
            Event::File(FileEvent::Created { path }) | Event::File(FileEvent::Changed { path }) => {
                assert!(path.ends_with("a.txt"), "{path:?}");
            }
            other => panic!("意外的事件：{other:?}"),
        }
        assert!(
            fixture.next(WINDOW * 3).is_none(),
            "一次写入不该通知两次：窗口该把它收成一条"
        );
    }

    #[test]
    fn a_write_we_announced_does_not_come_back() {
        let mut fixture = Fixture::new("ours");
        fixture.watch();
        let ours = fixture.dir.join("ours.txt");
        // The write side's contract: announce before making the write.
        fixture
            .bus
            .publish(FileEvent::EditorWrote { path: ours.clone() });
        std::fs::write(&ours, "ours").unwrap();
        // A second file nobody announced, used as a fence: whatever is published for it arrives
        // after everything queued before it, so anything about our own file would already be in.
        std::fs::write(fixture.dir.join("theirs.txt"), "theirs").unwrap();

        let seen = fixture.until("theirs.txt", Duration::from_secs(5));
        assert!(
            seen.iter().any(|e| file_path(e).ends_with("theirs.txt")),
            "栅栏本身该到：{seen:?}"
        );
        assert!(
            !seen.iter().any(|e| file_path(e).ends_with("ours.txt")),
            "自己写的那条不该回放：{seen:?}"
        );
    }

    #[test]
    fn a_build_directory_stays_quiet() {
        let mut fixture = Fixture::new("ignored");
        std::fs::create_dir_all(fixture.dir.join("target/debug")).unwrap();
        fixture.watch();

        std::fs::write(fixture.dir.join("target/debug/out"), "binary").unwrap();
        std::fs::write(fixture.dir.join("kept.rs"), "fn main() {}").unwrap();

        let seen = fixture.until("kept.rs", Duration::from_secs(5));
        assert!(
            seen.iter().any(|e| file_path(e).ends_with("kept.rs")),
            "该看的改动要到：{seen:?}"
        );
        assert!(
            !seen.iter().any(|e| file_path(e).contains("target/")),
            "构建产物不该出现：{seen:?}"
        );
    }
}
