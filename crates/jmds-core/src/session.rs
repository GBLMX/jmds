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

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

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

/// A session on disk: where it is, and what its header says.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub path: PathBuf,
    pub header: Header,
}

impl Summary {
    /// The id, which is what a person types to name this session.
    pub fn id(&self) -> &str {
        &self.header.id
    }
}

/// Read a session file's header line and nothing else.
///
/// Cheap on purpose: choosing a session means looking at every candidate, and reading whole files to
/// choose one would make the choice cost more than the thing chosen. Synchronous for the same reason
/// — it is a directory listing and a few short reads, and the callers are a menu that has to answer
/// *now* as much as a startup that could wait.
pub fn summary(path: &Path) -> std::io::Result<Summary> {
    use std::io::BufRead;

    let file = std::fs::File::open(path)?;
    for raw in std::io::BufReader::new(file).lines() {
        let raw = raw?;
        if raw.trim().is_empty() {
            continue;
        }
        // The first line that carries anything has to be the header: a session file without one is
        // not a session file, and guessing at the rest of it would be worse than saying so.
        if let Ok(Line::Header(header)) = serde_json::from_str::<Line>(&raw) {
            return Ok(Summary {
                path: path.to_path_buf(),
                header,
            });
        }
        break;
    }
    Err(std::io::Error::new(
        ErrorKind::InvalidData,
        format!("{} is not a session file", path.display()),
    ))
}

/// Every session file in `dir`, newest first. A directory that is not there yet is an app that has
/// never been run, not an error.
///
/// Ordering is by id, which starts with the unix millisecond the session was created at: that is
/// what "which conversation is the latest" asks, it costs no `stat`, and it does not move when an
/// old session is reopened — reopening a conversation does not make it new.
pub fn list(dir: &Path) -> std::io::Result<Vec<Summary>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut summaries = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        // One file that is not a session — a stray `.jsonl`, a session that never got a header —
        // is skipped rather than fatal: a bad file should not hide every good one.
        if let Ok(summary) = summary(&path) {
            summaries.push(summary);
        }
    }
    summaries.sort_by(|left, right| compare_ids(right.id(), left.id()));
    Ok(summaries)
}

/// One session by id, from `dir`.
pub fn by_id(dir: &Path, id: &str) -> std::io::Result<Option<Summary>> {
    match summary(&dir.join(format!("{id}.jsonl"))) {
        Ok(summary) => Ok(Some(summary)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The most recent session held in `cwd`.
///
/// Resuming another project's conversation is worse than resuming nothing: the model would be handed
/// a history about files it cannot see. So the working directory has to match, and sessions that do
/// not are skipped rather than offered.
pub fn latest_in(dir: &Path, cwd: &Path) -> std::io::Result<Option<Summary>> {
    Ok(list(dir)?
        .into_iter()
        .find(|summary| summary.header.cwd == cwd))
}

/// Order two session ids: the millisecond they were created at, then the within-millisecond counter.
/// An id that does not parse sorts after ones that do, by text, so a hand-made file lands at the end
/// of the list instead of somewhere in the middle of it.
fn compare_ids(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (parse_id(left), parse_id(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

fn parse_id(id: &str) -> Option<(u64, u64)> {
    let (millis, counter) = id.split_once('-')?;
    Some((millis.parse().ok()?, counter.parse().ok()?))
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
        // A counter as well as the name. Two tests asking for the same name would share one
        // directory, and each one's `remove_dir_all` would delete the other's fixtures halfway
        // through — a failure that only appears when the tests run in parallel, which is how they
        // run, and which looks like a bug in whatever the test was checking.
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "jmds-session-{}-{name}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A session file with just its header — which is all choosing one needs.
    async fn session(dir: &Path, id: &str, cwd: &str) -> PathBuf {
        SessionFile::create(dir, Header::new(id, "test-model", cwd))
            .await
            .unwrap()
            .path()
            .to_path_buf()
    }

    #[tokio::test]
    async fn a_header_is_read_without_reading_the_rest() {
        let dir = scratch("header-only");
        let path = dir.join("77-0.jsonl");
        let header = serde_json::to_string(&Line::Header(Header::new("77-0", "m", "/w"))).unwrap();
        // A half-written line after the header is exactly the state a crashed session is in.
        std::fs::write(&path, format!("{header}\nnot json at all\n")).unwrap();

        let summary = summary(&path).expect("一个头");
        assert_eq!(summary.id(), "77-0");
        assert_eq!(summary.header.cwd, PathBuf::from("/w"));
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_session_is_not_one_and_hides_nothing() {
        let dir = scratch("stray-jsonl");
        let strange = dir.join("notes.jsonl");
        std::fs::write(&strange, "not json at all\n").unwrap();
        assert!(summary(&strange).is_err());

        session(&dir, "100-0", "/w").await;
        let listed = list(&dir).unwrap();
        assert_eq!(listed.len(), 1, "坏文件不该把好文件一起藏起来");
        assert_eq!(listed[0].id(), "100-0");
    }

    #[tokio::test]
    async fn sessions_are_listed_newest_first_within_the_same_millisecond_too() {
        let dir = scratch("order");
        for id in ["100-0", "200-0", "200-1", "99-9"] {
            session(&dir, id, "/w").await;
        }
        std::fs::write(dir.join("notes.txt"), "x").unwrap();

        let ids: Vec<String> = list(&dir)
            .unwrap()
            .into_iter()
            .map(|summary| summary.header.id)
            .collect();
        assert_eq!(ids, ["200-1", "200-0", "100-0", "99-9"]);
    }

    #[tokio::test]
    async fn a_hand_made_id_lands_at_the_end_of_the_list() {
        let dir = scratch("weird-id");
        session(&dir, "handmade", "/w").await;
        session(&dir, "100-0", "/w").await;

        let ids: Vec<String> = list(&dir)
            .unwrap()
            .into_iter()
            .map(|summary| summary.header.id)
            .collect();
        assert_eq!(ids, ["100-0", "handmade"]);
    }

    #[tokio::test]
    async fn resuming_only_offers_this_projects_conversation() {
        let dir = scratch("cwd");
        session(&dir, "100-0", "/project/a").await;
        // Newer, but about somewhere else: offered to nobody standing here.
        session(&dir, "200-0", "/project/b").await;

        let found = latest_in(&dir, Path::new("/project/a"))
            .unwrap()
            .expect("a 项目的会话");
        assert_eq!(found.id(), "100-0");
        assert!(
            latest_in(&dir, Path::new("/project/c")).unwrap().is_none(),
            "没在这个项目里聊过，就没有可接的话"
        );
    }

    #[tokio::test]
    async fn a_session_is_found_by_id_and_a_missing_one_is_not_an_error() {
        let dir = scratch("by-id");
        session(&dir, "42-7", "/w").await;

        assert_eq!(by_id(&dir, "42-7").unwrap().expect("找到了").id(), "42-7");
        assert!(by_id(&dir, "nope").unwrap().is_none());
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
