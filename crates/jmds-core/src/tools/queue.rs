//! One writer per file.
//!
//! A turn can ask for several changes to the same file at once — the model emits two `edit` calls,
//! or a `write` after an `edit` — and without a queue they interleave: both read the original, both
//! write, and the first one's work is gone. Reads do not need this (they do not change anything);
//! everything that writes does.
//!
//! The queue is keyed by the file's **real** path. Two names for one file are one file: a symlink,
//! a `..`, a relative path joined onto the working directory. Keying by the string the model passed
//! would give two locks for one file, which is the same as no lock at all.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Debug, Default)]
pub struct FileMutex {
    held: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

/// The file's turn, held for as long as this value lives.
pub struct FileLock {
    _guard: OwnedMutexGuard<()>,
}

impl FileMutex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait for the file's turn.
    ///
    /// Held while the caller holds the returned value, so the discipline is visible at the call
    /// site: `let _turn = files.lock(&path).await;` and then do the read-modify-write.
    pub async fn lock(&self, path: &Path) -> FileLock {
        let key = key_for(path);
        let cell = {
            let mut held = self.held.lock().await;
            // An entry nobody holds and nobody waits on is dead weight, and a session touches many
            // files: drop those before adding another. `strong_count == 1` means only the map
            // refers to it — no guard, no waiter.
            held.retain(|_, cell| Arc::strong_count(cell) > 1);
            held.entry(key).or_default().clone()
        };
        FileLock {
            _guard: cell.lock_owned().await,
        }
    }

    /// How many files the map is tracking. Exposed for the test that it does not grow forever.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.held
            .try_lock()
            .map(|held| held.len())
            .unwrap_or_default()
    }
}

/// The path a lock is keyed by: the real one.
///
/// `canonicalize` resolves `..`, symlinks and relative pieces — and fails for a file that does not
/// exist yet, which is exactly what a `write` is about to create. The fallback then resolves the
/// containing directory and keeps the file name, which is enough for two spellings of the same new
/// file to still meet.
fn key_for(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => dir
            .canonicalize()
            .map(|dir| dir.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-queue-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Whether a lock can be taken right now, without waiting.
    async fn is_free(files: &FileMutex, path: &Path) -> bool {
        tokio::time::timeout(std::time::Duration::from_millis(50), files.lock(path))
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn the_same_file_waits_its_turn() {
        let dir = scratch("same");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let files = FileMutex::new();

        let first = files.lock(&file).await;
        assert!(
            !is_free(&files, &file).await,
            "a second writer must wait for the first"
        );
        drop(first);
        assert!(is_free(&files, &file).await, "and then get its turn");
    }

    #[tokio::test]
    async fn two_names_for_one_file_share_one_lock() {
        let dir = scratch("names");
        let sub = dir.join("sub");
        let _ = std::fs::create_dir_all(&sub);
        let file = dir.join("b.txt");
        std::fs::write(&file, b"x").unwrap();
        let files = FileMutex::new();

        // The same file, spelled with a detour through a subdirectory.
        let other_name = sub.join("..").join("b.txt");
        assert_eq!(
            key_for(&file),
            key_for(&other_name),
            "canonicalisation is what makes the two spellings meet"
        );
        let _first = files.lock(&file).await;
        assert!(!is_free(&files, &other_name).await);
    }

    #[tokio::test]
    async fn different_files_do_not_wait_for_each_other() {
        let dir = scratch("different");
        let files = FileMutex::new();
        let _a = files.lock(&dir.join("one.txt")).await;
        assert!(
            is_free(&files, &dir.join("two.txt")).await,
            "edits to different files are independent work"
        );
    }

    #[tokio::test]
    async fn a_file_that_does_not_exist_yet_is_keyed_by_its_directory() {
        let dir = scratch("new");
        let files = FileMutex::new();
        let new_file = dir.join("created-later.txt");

        let first = files.lock(&new_file).await;
        assert!(!is_free(&files, &new_file).await, "the same new file waits");
        assert!(
            is_free(&files, &dir.join("another-new.txt")).await,
            "a different new file does not"
        );
        drop(first);
    }

    #[tokio::test]
    async fn the_map_does_not_keep_every_file_forever() {
        let dir = scratch("growth");
        let files = FileMutex::new();
        for i in 0..8 {
            let _lock = files.lock(&dir.join(format!("file-{i}.txt"))).await;
        }
        // Seven of the eight were released before the last one was taken, so they are pruned on
        // the way in; the point is that the count tracks work in flight, not work ever done.
        assert!(files.tracked() <= 2, "tracked {}", files.tracked());
    }

    #[tokio::test]
    async fn two_writers_on_one_file_never_overlap() {
        // The property the queue exists for, stated as a property: from inside the critical
        // section, no other writer is inside it.
        let dir = scratch("overlap");
        let file = dir.join("c.txt");
        std::fs::write(&file, b"x").unwrap();
        let files = Arc::new(FileMutex::new());
        let inside = Arc::new(AtomicUsize::new(0));
        let worst = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let files = files.clone();
            let inside = inside.clone();
            let worst = worst.clone();
            let file = file.clone();
            tasks.push(tokio::spawn(async move {
                for _ in 0..4 {
                    let _turn = files.lock(&file).await;
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    worst.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(worst.load(Ordering::SeqCst), 1, "two writers were inside");
    }
}
