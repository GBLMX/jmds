//! The `bash` tool: one command, bounded output, and an honest exit status.
//!
//! What it is: a **non-interactive** shell invocation — stdin is closed, the two output streams are
//! read together, and the call ends when the command does. No PTY: a program that wants to ask a
//! question would hang the agent loop, and the human's interactive shell is a pane, not a tool call.
//! (A `pty` flag can come later; nothing here would have to change shape for it.)
//!
//! Three decisions worth stating:
//!
//! - **The output is the *tail*.** A build's errors are at the end, and a command that prints a
//!   hundred thousand lines has its interesting ones last. Everything is still on disk: once the
//!   output crosses the in-memory budget it is mirrored to an artifact and the answer names it, so
//!   nothing is lost — the model is told where the rest is rather than being handed a fragment.
//! - **A timeout is data, and being overruled is said out loud.** The default, the floor and the
//!   ceiling live in one table below; `0` means "no deadline" explicitly; and when a requested
//!   timeout is clamped the answer carries a line saying so, because silently running for less time
//!   than the model asked for is how a tool lies.
//! - **The child gets its own process group, and a timeout kills the group.** Killing `bash` alone
//!   leaves whatever it started running — a `cargo test`, a `sleep` — holding the terminal and the
//!   log file. This is the one place this module needs Unix; elsewhere it is portable.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, process::Command};

use super::truncate::{self, MAX_BYTES, MAX_COLUMN, MAX_LINES};

/// Seconds a command may run when the model does not say.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// The shortest deadline a caller may ask for. Zero is not "a very short time" — it is "no deadline",
/// and one second is the shortest that means anything.
pub const MIN_TIMEOUT_SECS: u64 = 1;
/// The longest deadline a caller may ask for, however large a number it sends.
pub const MAX_TIMEOUT_SECS: u64 = 3600;

/// How much output is kept in memory before it starts going to an artifact instead.
const MEMORY_BUDGET: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashArgs {
    pub command: String,
    /// Seconds to allow. Omitted means [`DEFAULT_TIMEOUT_SECS`]; `0` means no deadline at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// Where to run it. Relative paths are resolved against the session's working directory. When
    /// omitted, a leading `cd <dir> && ` in the command is used instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl BashArgs {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout: None,
            cwd: None,
        }
    }

    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command line to run, through `bash -c`. It is not interactive: stdin is closed. Prefer one command that prints what you need over several that each print a little."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Seconds to allow. Omitted means 300. `0` means no deadline; anything above 3600 is run as 3600 and the answer will say so."
                },
                "cwd": {
                    "type": "string",
                    "description": "Directory to run in. Relative paths resolve against the session directory. Omitted: a leading `cd <dir> && ` in the command is used."
                }
            },
            "required": ["command"]
        })
    }
}

/// What a call's timeout came to, and whether the caller had to be overruled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout {
    /// `None` is "no deadline": the command runs until it finishes.
    pub secs: Option<u64>,
    /// `(requested, used)` when the request did not survive the table.
    pub clamped: Option<(u64, u64)>,
}

/// Resolve a requested timeout against the table above.
pub fn resolve_timeout(requested: Option<u64>) -> Timeout {
    match requested {
        // No deadline, at the caller's explicit request.
        Some(0) => Timeout {
            secs: None,
            clamped: None,
        },
        None => Timeout {
            secs: Some(DEFAULT_TIMEOUT_SECS),
            clamped: None,
        },
        Some(secs) if secs < MIN_TIMEOUT_SECS => Timeout {
            secs: Some(MIN_TIMEOUT_SECS),
            clamped: Some((secs, MIN_TIMEOUT_SECS)),
        },
        Some(secs) if secs > MAX_TIMEOUT_SECS => Timeout {
            secs: Some(MAX_TIMEOUT_SECS),
            clamped: Some((secs, MAX_TIMEOUT_SECS)),
        },
        Some(secs) => Timeout {
            secs: Some(secs),
            clamped: None,
        },
    }
}

#[derive(Debug)]
pub enum BashError {
    BadCwd { path: PathBuf, reason: String },
    Spawn(String),
}

impl std::fmt::Display for BashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadCwd { path, reason } => {
                write!(f, "working directory {} {}", path.display(), reason)
            }
            Self::Spawn(error) => write!(f, "the command could not be started: {error}"),
        }
    }
}

impl std::error::Error for BashError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashOutput {
    /// The command's tail, with long lines capped.
    pub text: truncate::Truncated,
    pub exit_code: Option<i32>,
    /// Set when the process died from a signal (128 + this is the shell's report).
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cwd: PathBuf,
    /// Set when the output was too long to keep in memory: the file holding all of it.
    pub artifact: Option<PathBuf>,
    /// Set when that file could not be written, so the model is not left believing the rest is
    /// somewhere it can read.
    pub artifact_error: Option<String>,
    pub clamp_notice: Option<String>,
}

impl BashOutput {
    /// Whether this is a failure as far as the model is concerned.
    pub fn is_error(&self) -> bool {
        self.timed_out || self.signal.is_some() || self.exit_code.is_some_and(|code| code != 0)
    }

    /// The answer: the output, then one line about how it ended.
    pub fn render(&self) -> String {
        let mut out = self.text.text.clone();
        let mut notes = Vec::new();

        if self.timed_out {
            notes.push("timed out and was killed".to_string());
        } else if let Some(signal) = self.signal {
            notes.push(format!("killed by signal {signal}"));
        } else if let Some(code) = self.exit_code {
            notes.push(format!("exit {code}"));
        }
        if !self.text.complete {
            notes.push(format!(
                "showing lines {}-{} of {}",
                self.text.first_line, self.text.last_line, self.text.total_lines
            ));
        }
        if let Some(artifact) = &self.artifact {
            notes.push(format!("full output: {}", artifact.display()));
        } else if let Some(error) = &self.artifact_error {
            notes.push(format!(
                "the rest of the output could not be saved: {error}"
            ));
        }
        if let Some(notice) = &self.clamp_notice {
            notes.push(notice.clone());
        }

        if !notes.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("[{}]", notes.join(" · ")));
        }
        out
    }
}

/// Run one command.
///
/// `session_cwd` is the directory the session is working in; a relative `cwd` resolves against it,
/// and it is where a command runs when neither `cwd` nor a leading `cd` says otherwise.
pub async fn bash(args: &BashArgs, session_cwd: &Path) -> Result<BashOutput, BashError> {
    let (command, cwd) = resolve_command(args, session_cwd)?;
    let timeout = resolve_timeout(args.timeout);

    let mut child = Command::new("bash")
        .arg("-c")
        .arg(&command)
        .current_dir(&cwd)
        // Closed on purpose: an interactive command would wait for input that never comes and the
        // call would sit there until its timeout.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| BashError::Spawn(error.to_string()))?;

    let sink = Arc::new(Sink::new());
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let reader_a = tokio::spawn(read_into(stdout, sink.clone()));
    let reader_b = tokio::spawn(read_into(stderr, sink.clone()));

    let status = match timeout.secs {
        Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), child.wait()).await {
            Ok(status) => status.map_err(|error| BashError::Spawn(error.to_string()))?,
            Err(_) => {
                kill_group(&child);
                let _ = child.wait().await;
                let _ = reader_a.await;
                let _ = reader_b.await;
                let text = sink.tail();
                return Ok(BashOutput {
                    text,
                    exit_code: None,
                    signal: Some(9),
                    timed_out: true,
                    cwd,
                    artifact: sink.artifact(),
                    artifact_error: sink.artifact_error(),
                    clamp_notice: clamp_notice(timeout),
                });
            }
        },
        None => child
            .wait()
            .await
            .map_err(|error| BashError::Spawn(error.to_string()))?,
    };

    let _ = reader_a.await;
    let _ = reader_b.await;

    let (exit_code, signal) = exit_status(&status);
    Ok(BashOutput {
        text: sink.tail(),
        exit_code,
        signal,
        timed_out: false,
        cwd,
        artifact: sink.artifact(),
        artifact_error: sink.artifact_error(),
        clamp_notice: clamp_notice(timeout),
    })
}

/// Work out what to run and where.
///
/// An explicit `cwd` wins. Otherwise a leading `cd <dir> &&` is lifted out of the command: models
/// write it that way constantly, and running it *as part of* the command means the reported working
/// directory would be a lie.
fn resolve_command(args: &BashArgs, session_cwd: &Path) -> Result<(String, PathBuf), BashError> {
    let (command, cwd) = match &args.cwd {
        Some(dir) => (args.command.clone(), Some(dir.clone())),
        None => match lift_leading_cd(&args.command) {
            Some((dir, rest)) => (rest, Some(dir)),
            None => (args.command.clone(), None),
        },
    };

    let cwd = match cwd {
        // With no cwd of its own, the child runs where the session does; the default is not spelled
        // out in the reported path so a relative session directory stays relative to the user.
        None => session_cwd.to_path_buf(),
        Some(dir) => {
            let expanded = crate::paths::expand_tilde(&dir);
            if expanded.is_absolute() {
                expanded
            } else {
                session_cwd.join(expanded)
            }
        }
    };

    let metadata = std::fs::metadata(&cwd).map_err(|error| BashError::BadCwd {
        path: cwd.clone(),
        reason: match error.kind() {
            std::io::ErrorKind::NotFound => "does not exist".to_string(),
            _ => format!("could not be inspected: {error}"),
        },
    })?;
    if !metadata.is_dir() {
        return Err(BashError::BadCwd {
            path: cwd,
            reason: "is not a directory".to_string(),
        });
    }

    Ok((command, cwd))
}

/// `cd dir && rest` → `(dir, rest)`, without touching anything else.
fn lift_leading_cd(command: &str) -> Option<(String, String)> {
    let trimmed = command.trim_start();
    let rest = trimmed.strip_prefix("cd ")?;
    let (dir, tail) = match rest.find("&&") {
        Some(at) => (&rest[..at], rest[at + 2..].trim_start()),
        None => (rest, ""),
    };
    let dir = dir.trim().trim_matches('"').trim_matches('\'').trim();
    if dir.is_empty() {
        return None;
    }
    Some((dir.to_string(), tail.to_string()))
}

fn clamp_notice(timeout: Timeout) -> Option<String> {
    timeout
        .clamped
        .map(|(requested, used)| format!("timeout clamped to {used}s (asked for {requested}s)"))
}

fn exit_status(status: &std::process::ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code(), None)
    }
}

/// Kill the child's whole process group, so what it started dies with it.
fn kill_group(child: &tokio::process::Child) {
    let Some(pid) = child.id() else {
        return;
    };
    #[cfg(unix)]
    {
        // The child was made a group leader by `process_group(0)`, so the negative pid addresses
        // the group. SIGKILL rather than SIGTERM: a test run that ignores SIGTERM must still die
        // before the tool returns.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// The running total of a command's output: everything goes to the artifact once the in-memory
/// budget is crossed, and the tail of it is what the model is shown.
struct Sink {
    inner: Mutex<SinkInner>,
}

struct SinkInner {
    /// The tail, bounded by [`MEMORY_BUDGET`].
    text: String,
    /// The artifact file, once the output outgrew memory. `None` while it still fits.
    artifact: Option<std::fs::File>,
    artifact_path: Option<PathBuf>,
    total_bytes: usize,
    /// Newlines seen, counted as the output arrives. Counting them here rather than re-counting the
    /// retained tail is what lets the footer say "of 40000" when only the last screen is in memory:
    /// a count derived from the buffer would report the size of what is left, not of what ran.
    newlines: usize,
    /// Whether the head was dropped to stay inside the budget.
    head_dropped: bool,
    /// Why the artifact could not be written, if it could not be.
    artifact_error: Option<String>,
}

impl Sink {
    fn new() -> Self {
        Self {
            inner: Mutex::new(SinkInner {
                text: String::new(),
                artifact: None,
                artifact_path: None,
                total_bytes: 0,
                newlines: 0,
                head_dropped: false,
                artifact_error: None,
            }),
        }
    }

    fn push(&self, chunk: &[u8]) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.total_bytes += chunk.len();
        inner.newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
        let text = String::from_utf8_lossy(chunk);

        // Once the budget is crossed, the whole thing goes to a file and keeps growing there: the
        // model gets the tail, and everyone gets the rest by name.
        if inner.artifact.is_none()
            && inner.artifact_error.is_none()
            && inner.text.len() + text.len() > MEMORY_BUDGET
        {
            let path = artifact_path();
            // The cache directory may not exist on a machine that has never run this: create it,
            // and if even that fails say so rather than dropping the output on the floor.
            let created = path
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()))
                .and_then(|()| std::fs::File::create(&path));
            match created {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = file.write_all(inner.text.as_bytes());
                    inner.artifact_path = Some(path);
                    inner.artifact = Some(file);
                }
                Err(error) => inner.artifact_error = Some(error.to_string()),
            }
        }

        if let Some(file) = inner.artifact.as_mut() {
            use std::io::Write;
            let _ = file.write_all(text.as_bytes());
        }
        inner.text.push_str(&text);

        // Keep the tail only: the head has already been written to the artifact if there is one.
        if inner.text.len() > MEMORY_BUDGET {
            let mut cut = inner.text.len() - MEMORY_BUDGET;
            while cut < inner.text.len() && !inner.text.is_char_boundary(cut) {
                cut += 1;
            }
            inner.text.drain(..cut);
            inner.head_dropped = true;
        }
    }

    fn tail(&self) -> truncate::Truncated {
        let Ok(inner) = self.inner.lock() else {
            return truncate::Truncated {
                text: String::new(),
                first_line: 1,
                last_line: 0,
                total_lines: 0,
                complete: true,
                first_line_overflows: false,
            };
        };

        // A command's long lines are capped rather than refused: a log with one enormous line
        // should still show the lines after it.
        let capped = truncate::cap_columns(&inner.text, MAX_COLUMN);
        let shown = truncate::tail(&capped, MAX_LINES, MAX_BYTES);

        // The counts the model is told are the whole run's, not the retained tail's.
        let total_lines =
            inner.newlines + usize::from(!inner.text.is_empty() && !inner.text.ends_with('\n'));
        let shown_lines = shown.text.lines().count();
        truncate::Truncated {
            first_line: total_lines.saturating_sub(shown_lines) + 1,
            last_line: total_lines,
            total_lines,
            complete: shown.complete && !inner.head_dropped,
            ..shown
        }
    }

    fn artifact(&self) -> Option<PathBuf> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.artifact_path.clone())
    }

    fn artifact_error(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.artifact_error.clone())
    }
}

async fn read_into<R: AsyncReadExt + Unpin>(mut reader: R, sink: Arc<Sink>) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => sink.push(&buffer[..read]),
        }
    }
}

/// A unique file name for this run's overflow, without a random-number dependency.
fn artifact_path() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    crate::paths::artifacts_dir().join(format!("bash-{stamp}-{n}.log"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-bash-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_command_that_works_reports_its_output_and_its_exit() {
        let dir = scratch("ok");
        let out = bash(&BashArgs::new("echo hello"), &dir).await.unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.is_error());
        assert_eq!(out.text.text.trim(), "hello");
        assert_eq!(out.cwd, dir);
        assert!(out.artifact.is_none());
        assert!(out.render().ends_with("[exit 0]"), "{}", out.render());
    }

    #[tokio::test]
    async fn a_failing_command_is_an_error_with_its_code() {
        let dir = scratch("fail");
        let out = bash(&BashArgs::new("exit 3"), &dir).await.unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert!(out.is_error());
        assert!(out.render().contains("[exit 3]"));
    }

    #[tokio::test]
    async fn both_streams_are_read_together() {
        let dir = scratch("streams");
        let out = bash(&BashArgs::new("echo out; echo err 1>&2"), &dir)
            .await
            .unwrap();
        let text = out.text.text;
        assert!(text.contains("out"), "{text}");
        assert!(text.contains("err"), "{text}");
    }

    #[tokio::test]
    async fn a_relative_cwd_is_resolved_against_the_session_directory() {
        let dir = scratch("cwd");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let mut args = BashArgs::new("pwd");
        args.cwd = Some("sub".into());
        let out = bash(&args, &dir).await.unwrap();
        assert_eq!(out.cwd, dir.join("sub"));
        assert!(out.text.text.trim().ends_with("sub"), "{}", out.text.text);
    }

    #[tokio::test]
    async fn a_leading_cd_is_lifted_out_of_the_command() {
        // Models write `cd somewhere && do it` constantly. Running that as written would make the
        // reported working directory a lie, so it becomes the cwd instead.
        let dir = scratch("leading-cd");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let out = bash(&BashArgs::new("cd sub && pwd"), &dir).await.unwrap();
        assert_eq!(out.cwd, dir.join("sub"));
        assert_eq!(out.text.text.trim(), dir.join("sub").to_string_lossy());

        // And a command that is only a cd still runs, in that directory.
        let out = bash(&BashArgs::new("cd sub"), &dir).await.unwrap();
        assert_eq!(out.cwd, dir.join("sub"));
    }

    #[tokio::test]
    async fn a_working_directory_that_is_wrong_is_reported_with_the_reason() {
        let dir = scratch("bad-cwd");
        let mut args = BashArgs::new("pwd");
        args.cwd = Some("nowhere".into());
        let error = bash(&args, &dir).await.unwrap_err();
        assert!(matches!(error, BashError::BadCwd { .. }));
        assert!(error.to_string().contains("does not exist"), "{error}");

        let file = dir.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        args.cwd = Some(file.to_string_lossy().into());
        let error = bash(&args, &dir).await.unwrap_err();
        assert!(error.to_string().contains("is not a directory"), "{error}");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_command_and_says_so() {
        let dir = scratch("timeout");
        let mut args = BashArgs::new("sleep 30");
        args.timeout = Some(1);
        let started = std::time::Instant::now();
        let out = bash(&args, &dir).await.unwrap();
        assert!(out.timed_out);
        assert!(out.is_error());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the call waited for the command instead of killing it: {:?}",
            started.elapsed()
        );
        assert!(out.render().contains("timed out"), "{}", out.render());
    }

    #[tokio::test]
    async fn a_zero_timeout_means_no_deadline() {
        let dir = scratch("no-deadline");
        let mut args = BashArgs::new("sleep 1; echo done");
        args.timeout = Some(0);
        let out = bash(&args, &dir).await.unwrap();
        assert!(!out.timed_out);
        assert_eq!(out.text.text.trim(), "done");
    }

    #[test]
    fn the_timeout_table_clamps_both_ends_and_says_so() {
        assert_eq!(resolve_timeout(None).secs, Some(DEFAULT_TIMEOUT_SECS));
        assert_eq!(resolve_timeout(None).clamped, None);
        assert_eq!(
            resolve_timeout(Some(0)).secs,
            None,
            "0 is a request, not a zero"
        );
        assert_eq!(resolve_timeout(Some(30)).secs, Some(30));
        assert_eq!(
            resolve_timeout(Some(99_999)).clamped,
            Some((99_999, MAX_TIMEOUT_SECS))
        );
        assert_eq!(
            resolve_timeout(Some(1)).clamped,
            None,
            "the floor is allowed"
        );
    }

    #[tokio::test]
    async fn a_clamped_timeout_is_visible_to_the_model() {
        let dir = scratch("clamp");
        let mut args = BashArgs::new("echo quick");
        args.timeout = Some(99_999);
        let out = bash(&args, &dir).await.unwrap();
        assert!(
            out.render()
                .contains("timeout clamped to 3600s (asked for 99999s)"),
            "{}",
            out.render()
        );
    }

    #[tokio::test]
    async fn output_too_long_for_memory_is_kept_in_an_artifact_and_named() {
        let dir = scratch("artifact");
        let out = bash(
            &BashArgs::new("for i in $(seq 1 40000); do echo \"line $i\"; done"),
            &dir,
        )
        .await
        .unwrap();

        let artifact = out
            .artifact
            .clone()
            .expect("the output should have spilled");
        let full = std::fs::read_to_string(&artifact).unwrap();
        assert!(
            full.contains("line 1\n"),
            "the artifact holds the output from the start"
        );
        assert!(full.contains("line 40000"), "and to the end");
        assert_eq!(
            full.lines().count(),
            40000,
            "every line is in the artifact, not just the tail"
        );
        assert!(
            !out.text.complete,
            "the head was dropped, so the answer must not read as the whole output"
        );
        assert_eq!(
            out.text.total_lines, 40000,
            "the count is the run's, not the tail's"
        );
        assert!(out.render().contains("showing lines"), "{}", out.render());
        assert!(
            out.render().contains(&artifact.display().to_string()),
            "{}",
            out.render()
        );
        let _ = std::fs::remove_file(&artifact);
    }

    #[tokio::test]
    async fn the_artifact_directory_is_created_when_it_is_missing() {
        // The cache directory need not exist on a machine that has never run this. The output is
        // still kept, and `artifact_error` stays empty.
        let dir = scratch("artifact-dir");
        let before = crate::paths::artifacts_dir();
        let _ = std::fs::remove_dir_all(&before);
        let out = bash(
            &BashArgs::new("for i in $(seq 1 40000); do echo \"line $i\"; done"),
            &dir,
        )
        .await
        .unwrap();
        assert_eq!(out.artifact_error, None);
        let artifact = out.artifact.expect("the output should have spilled");
        assert!(artifact.exists());
        assert!(artifact.starts_with(&before), "{}", artifact.display());
        let _ = std::fs::remove_file(&artifact);
    }

    #[tokio::test]
    async fn a_single_enormous_line_is_capped_with_a_mark() {
        let dir = scratch("long-line");
        let out = bash(
            &BashArgs::new("for i in $(seq 1 2000); do printf 'x'; done; echo; echo after"),
            &dir,
        )
        .await
        .unwrap();
        let text = out.text.text;
        assert!(
            text.contains("[+"),
            "the cut is marked: {}",
            &text[..40.min(text.len())]
        );
        assert!(
            text.contains("after"),
            "the lines after a long one still show"
        );
    }

    #[test]
    fn the_schema_and_the_struct_describe_the_same_parameters() {
        let schema = BashArgs::schema();
        let mut in_schema: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        in_schema.sort_unstable();
        let mut args = BashArgs::new("x");
        args.timeout = Some(5);
        args.cwd = Some(".".into());
        let value = serde_json::to_value(&args).unwrap();
        let mut in_struct: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        in_struct.sort_unstable();
        assert_eq!(in_schema, in_struct);
        assert_eq!(schema["required"], serde_json::json!(["command"]));
    }
}
