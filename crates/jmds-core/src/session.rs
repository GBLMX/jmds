//! The session file: append-only JSONL, one line per thing that happened.
//!
//! Why a file of lines rather than a document that gets rewritten: a session is *appended to* while
//! it happens, and rewriting a growing document after every turn is work that grows with the
//! session and a chance to lose all of it to one bad write. Each line is written and flushed on its
//! own, so a crash costs at most the line being written.
//!
//! Recovery follows from that, and is deliberately **strict**: lines are read until one does not
//! parse, and reading stops there. It does not skip the bad line and carry on, because the order of
//! these lines *is* the contract — the messages are replayed to the provider in exactly this order
//! and the prefix cache is only hit if the prefix is byte-identical to last time. A hole in the
//! middle is not something to paper over; it is something to report and stop at.
//!
//! The I/O is `tokio`'s. A blocking write from inside the turn loop holds the runtime's thread for
//! as long as the disk takes, which is exactly the sort of stall that turns into a stutter in the
//! terminal — and the loop that writes these lines is async already.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use jmds_api::ChatMessage;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{File, OpenOptions},
    io::AsyncWriteExt,
};

use crate::event::TurnUsage;

/// What a session file says about itself, on its first line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// The session's id, which is also its file's stem.
    pub id: String,
    pub model: String,
    pub cwd: PathBuf,
    /// Unix milliseconds. Not a formatted date: formatting needs a calendar, and the calendar
    /// belongs in whatever shows a list of sessions, not in the record of one.
    pub started_at_ms: u64,
    /// The session this one was branched off, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branched_from: Option<String>,
}

impl Header {
    pub fn new(id: impl Into<String>, model: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            cwd: cwd.into(),
            started_at_ms: now_ms(),
            branched_from: None,
        }
    }
}

/// One line of a session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Line {
    Header(Header),
    /// A message, exactly as it was sent or received. This is what makes a session replayable.
    Message(ChatMessage),
    /// What one turn cost.
    Usage(TurnUsage),
    /// A line the transcript would have shown — an error, a restart, something the user did.
    Note {
        text: String,
    },
}

/// An open session file, appending.
#[derive(Debug)]
pub struct SessionFile {
    path: PathBuf,
    header: Header,
    file: File,
}

impl SessionFile {
    /// Start a new session in `dir`.
    pub async fn create(dir: &Path, header: Header) -> std::io::Result<Self> {
        tokio::fs::create_dir_all(dir).await?;
        let path = dir.join(format!("{}.jsonl", header.id));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .await?;
        let mut session = Self { path, header, file };
        session
            .append(&Line::Header(session.header.clone()))
            .await?;
        Ok(session)
    }

    /// Continue an existing session file.
    ///
    /// The header is read back rather than assumed, so a caller that opens the wrong file finds out
    /// here instead of by writing messages into a file that belongs to something else.
    pub async fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let recovered = recover(&path).await?;
        let header = recovered.header.ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidData,
                format!("{} does not start with a session header", path.display()),
            )
        })?;
        let file = OpenOptions::new().append(true).open(&path).await?;
        Ok(Self { path, header, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Write one line and flush it.
    ///
    /// The flush is the promise: a session that is still running is a session another process (or
    /// the next run of this one) can read up to the last complete line.
    pub async fn append(&mut self, line: &Line) -> std::io::Result<()> {
        let mut text = serde_json::to_string(line)
            .map_err(|error| std::io::Error::new(ErrorKind::InvalidData, error))?;
        text.push('\n');
        self.file.write_all(text.as_bytes()).await?;
        self.file.flush().await
    }

    /// Write the messages a turn added, in the order they were added.
    pub async fn append_messages(&mut self, messages: &[ChatMessage]) -> std::io::Result<usize> {
        for message in messages {
            self.append(&Line::Message(message.clone())).await?;
        }
        Ok(messages.len())
    }
}

/// What could be read back out of a session file.
#[derive(Debug, Clone, PartialEq)]
pub struct Recovered {
    /// The first line's header, when the file has one.
    pub header: Option<Header>,
    pub lines: Vec<Line>,
    /// Where reading stopped, 0-based, when it stopped before the end of the file.
    ///
    /// `None` means everything in the file was understood.
    pub stopped_at: Option<usize>,
}

impl Recovered {
    /// The conversation, in the order it happened.
    ///
    /// No reordering, no de-duplication and no rewriting of what the model sent: this is what gets
    /// replayed to the provider, and the prefix cache only hits if the prefix is what it was.
    pub fn messages(&self) -> Vec<ChatMessage> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Message(message) => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn usages(&self) -> Vec<TurnUsage> {
        self.lines
            .iter()
            .filter_map(|line| match line {
                Line::Usage(usage) => Some(*usage),
                _ => None,
            })
            .collect()
    }

    /// Whether the file was read to its end.
    pub fn is_complete(&self) -> bool {
        self.stopped_at.is_none()
    }
}

/// Read a session file, stopping at the first line that cannot be understood.
pub async fn recover(path: &Path) -> std::io::Result<Recovered> {
    let text = tokio::fs::read_to_string(path).await?;
    let mut recovered = Recovered {
        header: None,
        lines: Vec::new(),
        stopped_at: None,
    };

    for (index, raw) in text.lines().enumerate() {
        // Blank lines carry nothing. A half-written line is not blank — it has bytes that do not
        // parse — which is why the two cases are treated differently.
        if raw.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(raw) {
            Ok(line) => {
                if index == 0
                    && let Line::Header(header) = &line
                {
                    recovered.header = Some(header.clone());
                }
                recovered.lines.push(line);
            }
            Err(_) => {
                recovered.stopped_at = Some(index);
                break;
            }
        }
    }

    Ok(recovered)
}

/// Copy a session up to `keep` lines into a new one, so the branch starts from a decision point
/// instead of from the beginning.
pub async fn branch(path: &Path, keep: usize, new_id: &str) -> std::io::Result<PathBuf> {
    let recovered = recover(path).await?;
    let from = recovered.header.as_ref().map(|header| header.id.clone());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let target = dir.join(format!("{new_id}.jsonl"));

    let mut header = recovered.header.clone().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("{} is not a session file", path.display()),
        )
    })?;
    header.id = new_id.to_string();
    header.branched_from = from;

    let mut file = SessionFile::create(dir, header).await?;
    let kept: Vec<Line> = recovered
        .lines
        .into_iter()
        .filter(|line| !matches!(line, Line::Header(_)))
        .take(keep)
        .collect();
    for line in &kept {
        file.append(line).await?;
    }
    Ok(target)
}

/// A session id that sorts by time and cannot collide within the same millisecond.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", now_ms())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-session-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn header(id: &str) -> Header {
        Header::new(id, "deepseek-chat", "/tmp/project")
    }

    #[tokio::test]
    async fn a_session_round_trips_its_messages_in_order() {
        let dir = scratch("round-trip");
        let mut file = SessionFile::create(&dir, header("one")).await.unwrap();
        let messages = vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("read a.txt"),
            ChatMessage::assistant_with_tools(
                "reading",
                vec![jmds_api::ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: jmds_api::ToolCallFunction {
                        name: "read".into(),
                        arguments: r#"{"path":"a.txt"}"#.into(),
                    },
                }],
            ),
            ChatMessage::tool("call_1", "hello\n"),
            ChatMessage::assistant("a.txt says hello"),
        ];
        file.append_messages(&messages).await.unwrap();

        let recovered = recover(file.path()).await.unwrap();
        assert!(recovered.is_complete());
        assert_eq!(recovered.header.as_ref().unwrap().id, "one");
        assert_eq!(recovered.messages(), messages, "same order, same shapes");
    }

    #[tokio::test]
    async fn every_line_is_on_disk_before_the_next_one_is_written() {
        // The promise that makes a crash survivable: a reader that never sees a flush still sees
        // everything up to the last complete line.
        let dir = scratch("flush");
        let mut file = SessionFile::create(&dir, header("flush")).await.unwrap();
        file.append_messages(&[ChatMessage::user("one")])
            .await
            .unwrap();

        let so_far = tokio::fs::read_to_string(file.path()).await.unwrap();
        assert!(
            so_far.contains("one"),
            "the first message is already readable"
        );
        assert_eq!(so_far.lines().count(), 2, "header and one message");

        file.append_messages(&[ChatMessage::user("two")])
            .await
            .unwrap();
        let after = tokio::fs::read_to_string(file.path()).await.unwrap();
        assert_eq!(after.lines().count(), 3);
    }

    #[tokio::test]
    async fn a_line_cut_in_half_by_a_crash_is_where_reading_stops() {
        let dir = scratch("crash");
        let mut file = SessionFile::create(&dir, header("crash")).await.unwrap();
        file.append_messages(&[
            ChatMessage::user("one"),
            ChatMessage::user("two"),
            ChatMessage::user("three"),
        ])
        .await
        .unwrap();
        let path = file.path().to_path_buf();
        drop(file);

        // What a crash mid-write leaves behind: a line with no end.
        let mut raw = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        raw.write_all(br#"{"kind":"message","role":"user","conte"#)
            .await
            .unwrap();
        raw.flush().await.unwrap();

        let recovered = recover(&path).await.unwrap();
        assert_eq!(recovered.messages().len(), 3, "the three written messages");
        assert_eq!(
            recovered.stopped_at,
            Some(4),
            "the header, then three lines"
        );
        assert!(!recovered.is_complete());
    }

    #[tokio::test]
    async fn a_line_that_cannot_be_read_stops_reading_rather_than_being_skipped() {
        // A hole in the middle would break the replay the prefix cache depends on, so the rule is
        // to stop and say where — not to carry on with a gap.
        let dir = scratch("hole");
        let path = dir.join("hole.jsonl");
        let good = serde_json::to_string(&Line::Message(ChatMessage::user("one"))).unwrap();
        let body = format!(
            "{}\n{}\n{}\n{}\n",
            serde_json::to_string(&Line::Header(header("hole"))).unwrap(),
            good,
            "{ this is not json",
            good
        );
        tokio::fs::write(&path, body).await.unwrap();

        let recovered = recover(&path).await.unwrap();
        assert_eq!(recovered.messages().len(), 1, "nothing after the bad line");
        assert_eq!(recovered.stopped_at, Some(2));
    }

    #[tokio::test]
    async fn opening_a_file_that_is_not_a_session_says_so() {
        let dir = scratch("not-a-session");
        let path = dir.join("notes.jsonl");
        tokio::fs::write(&path, "just some text\n").await.unwrap();

        let error = SessionFile::open(&path).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains("session header"), "{error}");
    }

    #[tokio::test]
    async fn a_session_that_is_reopened_keeps_appending() {
        let dir = scratch("reopen");
        let mut file = SessionFile::create(&dir, header("reopen")).await.unwrap();
        file.append_messages(&[ChatMessage::user("before")])
            .await
            .unwrap();
        let path = file.path().to_path_buf();
        drop(file);

        let mut again = SessionFile::open(&path).await.unwrap();
        assert_eq!(again.header().id, "reopen");
        again
            .append_messages(&[ChatMessage::user("after")])
            .await
            .unwrap();

        let recovered = recover(&path).await.unwrap();
        assert_eq!(
            recovered
                .messages()
                .iter()
                .map(|message| message.content.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["before", "after"]
        );
        assert_eq!(
            recovered
                .lines
                .iter()
                .filter(|line| matches!(line, Line::Header(_)))
                .count(),
            1,
            "reopening does not write a second header"
        );
    }

    #[tokio::test]
    async fn a_branch_keeps_the_first_lines_and_says_where_it_came_from() {
        let dir = scratch("branch");
        let mut file = SessionFile::create(&dir, header("original")).await.unwrap();
        file.append_messages(&[
            ChatMessage::user("one"),
            ChatMessage::assistant("two"),
            ChatMessage::user("three"),
        ])
        .await
        .unwrap();
        let path = file.path().to_path_buf();
        drop(file);

        let branched = branch(&path, 2, "branch-1").await.unwrap();
        let recovered = recover(&branched).await.unwrap();
        assert_eq!(recovered.header.as_ref().unwrap().id, "branch-1");
        assert_eq!(
            recovered.header.as_ref().unwrap().branched_from.as_deref(),
            Some("original")
        );
        assert_eq!(
            recovered
                .messages()
                .iter()
                .map(|message| message.content.clone().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );

        // The original is untouched, and branching onto an existing id is refused.
        assert_eq!(recover(&path).await.unwrap().messages().len(), 3);
        let existing = tokio::fs::read_to_string(&branched).await.unwrap();
        assert!(
            branch(&path, 1, "branch-1").await.is_err(),
            "no overwriting"
        );
        assert_eq!(
            tokio::fs::read_to_string(&branched).await.unwrap(),
            existing
        );
    }

    #[tokio::test]
    async fn a_usage_line_is_kept_for_the_cost_total() {
        let dir = scratch("usage");
        let mut file = SessionFile::create(&dir, header("usage")).await.unwrap();
        let usage = TurnUsage {
            prompt_tokens: 100,
            cache_hit_tokens: 80,
            cache_miss_tokens: 20,
            completion_tokens: 7,
            reasoning_tokens: 3,
        };
        file.append(&Line::Usage(usage)).await.unwrap();

        let recovered = recover(file.path()).await.unwrap();
        assert_eq!(recovered.usages(), vec![usage]);
        assert!(recovered.messages().is_empty());
    }

    #[test]
    fn session_ids_sort_by_time_and_do_not_collide() {
        let first = new_id();
        let second = new_id();
        assert_ne!(first, second);
        let (a, b) = (
            first.split('-').next().unwrap(),
            second.split('-').next().unwrap(),
        );
        assert!(
            a <= b,
            "the timestamp part is the same or later: {first} {second}"
        );
        assert!(a.len() >= 12, "milliseconds, not seconds: {a}");
    }
}
