//! The four tools as the model sees them: one table, and one place a call lands.
//!
//! The descriptions live *here* rather than beside each tool's own docs, and that is deliberate:
//! what the model reads when it is choosing a tool is the prompt, and a prompt that is reviewed one
//! file at a time is a prompt nobody reviews. Four short paragraphs in one screen is the version
//! that gets read as a whole — and the thing they have to be read as a whole is *contrast*: the
//! model's real choice is `read` against `bash`, or `write` against `edit`, and that choice is made
//! by the differences between these paragraphs.
//!
//! A call arrives as a name and a JSON string, exactly as the API streams it. Parsing it into the
//! tool's argument struct happens here, so a malformed call becomes a message the model can act on
//! instead of a panic or a silently empty tool run — the raw arguments are handed back with
//! serde's complaint, which is usually enough for the model to fix its own call.
//!
//! The order of [`ORDER`] is fixed for the life of the process. The tool table is part of the
//! prompt prefix, and a prefix that reorders itself between turns is a prefix that never hits the
//! provider's cache.
//!
//! A `bash` call is also the one tool call someone can *watch*: the set opens a terminal pane for
//! it, mirrors the command's output into that pane, and takes `Ctrl+C` (or the pane being closed)
//! as "stop this call". That is deliberately here rather than in [`bash`]: a call's lifetime
//! belongs to whoever started it, and the tool set outlives the call — the pane has to be closed
//! by something that is still around when a *later* call needs the room.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

use jmds_api::ToolSpec;
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, oneshot};

use super::{bash, edit, queue::FileMutex, read, write};
use crate::{
    event::{Event, EventBus, FileEvent, PaneEvent, PtyEvent},
    pane::{PaneId, PaneKind, PaneSpec},
};

/// 同时留几个 `bash` 面板。
///
/// 两个：一格是这次调用，一格是上一次的 —— 人回头看「刚才跑的是什么」，看的就是上一格。再多
/// 就把布局挤没了，而这套布局里还有会话、编辑器和文件树。
const TOOL_PANES: usize = 2;

/// 面板里按下的 Ctrl+C，就是终端里的那个字节。
const CTRL_C: u8 = 0x03;

/// The tools, in the order the model is told about them.
pub const ORDER: [&str; 4] = ["read", "write", "edit", "bash"];

/// What the model is told about `read`.
const READ: &str = "\
Read a file, in whole lines. Give `path`, and optionally `offset` (1-based line to start at) and \
`limit` (how many lines). The answer says which lines it shows and, when there are more below, \
which `offset` to ask for next. Use this to see what is in a file before changing it: using `edit` \
on a file you have not read is how a change lands on text that is no longer there.";

/// What the model is told about `write`.
const WRITE: &str = "\
Put a whole file there, creating the directories it needs. `path` and `content`. This is the blunt \
tool: what it is given is what lands on disk, so it is for a file that should exist in full — a new \
file, a scratch note, a regenerated listing. To change part of a file that already exists, use \
`edit`; `write` would replace the whole thing, including the parts you did not look at.";

/// What the model is told about `edit`.
const EDIT: &str = "\
Change part of a file by replacing exact text. Give `path` and `edits`: a list of `old_text` and \
`new_text` pairs. Each `old_text` must appear **exactly once** in the file — if it appears twice, or \
not at all, the edit is refused and the answer says which. Every edit is matched against the file as \
it is on disk, not against the file as earlier edits in the same call would leave it, so the order in \
the list cannot change the result. Keep `old_text` as small as it can be while still being unique, \
and read the file first.";

/// What the model is told about `bash`.
const BASH: &str = "\
Run a shell command. `command`, optionally `timeout` in seconds (default 300, `0` means no \
deadline) and `cwd` (relative paths resolve against the session directory; a leading `cd <dir> &&` \
is honoured). This is not an interactive shell — stdin is closed, so a program that asks a question \
will hang until its timeout instead of reading an answer — and it runs one command, not a session, \
so a `cd` or an `export` does not survive into the next call. The answer shows the end of the \
output, its exit status, and where the whole of it was saved if it was long.";

/// The four tools, and the state they share.
pub struct ToolSet {
    /// The directory relative paths resolve against, and where `bash` runs by default.
    cwd: PathBuf,
    /// One writer per file, so two calls that touch the same path cannot interleave.
    files: FileMutex,
    /// Where to announce that a write is about to happen, when anyone is listening.
    ///
    /// Optional because it genuinely is: a tool set driven straight from a test has no bus, and a
    /// write nobody watches is exactly what such a test wants. See [`Self::announce_write`].
    bus: Option<EventBus>,
    /// 我开过的 `bash` 面板，最旧的在前。
    ///
    /// 记着是为了开新的之前把最旧的那一格关掉：模型连跑三十个命令，布局就淹了。上限是
    /// [`TOOL_PANES`]。没有总线时这份名单一直是空的 —— 没人能看的东西不必开出来。
    panes: parking_lot::Mutex<VecDeque<PaneId>>,
}

/// What one tool call produced.
///
/// Two strings because they go to two different readers: `summary` is one line for the transcript,
/// which a human reads while the turn is running, and `content` is what goes back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub ok: bool,
    pub summary: String,
    pub content: String,
}

impl ToolOutcome {
    pub fn done(summary: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            ok: true,
            summary: summary.into(),
            content: content.into(),
        }
    }

    pub fn failed(summary: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            ok: false,
            summary: summary.into(),
            content: content.into(),
        }
    }
}

impl ToolSet {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            files: FileMutex::new(),
            bus: None,
            panes: parking_lot::Mutex::new(VecDeque::new()),
        }
    }

    /// Announce this tool set's writes on `bus`.
    ///
    /// The watcher listens for [`FileEvent::EditorWrote`] and stays quiet about the notification
    /// that follows one: without this, every file the model writes comes back as a change from
    /// outside, and a pane that reloads on change reloads what the tool just wrote.
    pub fn with_bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Say that a write is about to happen. Called *before* the write, which is the whole contract:
    /// the announcement has to be on the bus by the time the filesystem notification comes back.
    fn announce_write(&self, path: impl AsRef<Path>) {
        if let Some(bus) = &self.bus {
            bus.publish(FileEvent::EditorWrote {
                path: path.as_ref().to_path_buf(),
            });
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The table the model is sent: names, descriptions, and each tool's own parameter schema.
    ///
    /// Built from the schemas rather than from a copy of them. A parameter the model is told about
    /// that nothing reads is worse than no parameter, and the way that happens is a second,
    /// hand-maintained copy of a schema.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let schema = |value: serde_json::Value| value;
        vec![
            ToolSpec {
                name: "read".into(),
                description: READ.into(),
                parameters: schema(read::ReadArgs::schema()),
            },
            ToolSpec {
                name: "write".into(),
                description: WRITE.into(),
                parameters: schema(write::WriteArgs::schema()),
            },
            ToolSpec {
                name: "edit".into(),
                description: EDIT.into(),
                parameters: schema(edit::EditArgs::schema()),
            },
            ToolSpec {
                name: "bash".into(),
                description: BASH.into(),
                parameters: schema(bash::BashArgs::schema()),
            },
        ]
    }

    /// A path as the session means it.
    ///
    /// A relative path reaches a tool as the model wrote it, and a tool that resolved it against
    /// the process's own directory would read and write somewhere the session is not — which is how
    /// a test file ends up in the source tree. Resolving here is also why `bash` is the only tool
    /// that needs the directory itself: it is the only one that runs something.
    fn resolve(&self, path: &str) -> String {
        let expanded = crate::paths::expand_tilde(path);
        if expanded.is_absolute() {
            expanded
        } else {
            self.cwd.join(expanded)
        }
        .to_string_lossy()
        .into_owned()
    }

    /// Run one call. Never fails: a tool that cannot run is a tool result the model has to read.
    ///
    /// One thin method per tool rather than a nested match: the dispatch is four lines, and each
    /// tool's parse, path resolution, summary and content read top to bottom in one place.
    pub async fn call(&self, name: &str, arguments: &str) -> ToolOutcome {
        match name {
            "read" => self.call_read(arguments).await,
            "write" => self.call_write(arguments).await,
            "edit" => self.call_edit(arguments).await,
            "bash" => self.call_bash(arguments).await,
            other => ToolOutcome::failed(
                format!("no tool called `{other}`"),
                format!(
                    "There is no tool called `{other}`. The tools are: {}.",
                    ORDER.join(", ")
                ),
            ),
        }
    }

    async fn call_read(&self, arguments: &str) -> ToolOutcome {
        let args: read::ReadArgs = match self.parse("read", arguments) {
            Ok(args) => args,
            Err(outcome) => return *outcome,
        };
        let args = read::ReadArgs {
            path: self.resolve(&args.path),
            ..args
        };
        match read::read(&args).await {
            Ok(out) => {
                let chapter = if out.shown.complete {
                    format!("{} lines", out.shown.total_lines)
                } else {
                    format!(
                        "lines {}-{} of {}",
                        out.shown.first_line, out.shown.last_line, out.shown.total_lines
                    )
                };
                ToolOutcome::done(
                    format!("read {} · {chapter}", out.path.display()),
                    out.render(),
                )
            }
            Err(error) => ToolOutcome::failed(format!("read failed: {error}"), error.to_string()),
        }
    }

    async fn call_write(&self, arguments: &str) -> ToolOutcome {
        let args: write::WriteArgs = match self.parse("write", arguments) {
            Ok(args) => args,
            Err(outcome) => return *outcome,
        };
        let args = write::WriteArgs {
            path: self.resolve(&args.path),
            ..args
        };
        self.announce_write(&args.path);
        match write::write(&args, &self.files).await {
            Ok(out) => {
                let verb = if out.created { "created" } else { "replaced" };
                let summary = format!("{verb} {} · {} bytes", out.path.display(), out.bytes);
                ToolOutcome::done(summary, format!("{verb} {}", out.path.display()))
            }
            Err(error) => ToolOutcome::failed(format!("write failed: {error}"), error.to_string()),
        }
    }

    async fn call_edit(&self, arguments: &str) -> ToolOutcome {
        let args: edit::EditArgs = match self.parse("edit", arguments) {
            Ok(args) => args,
            Err(outcome) => return *outcome,
        };
        let args = edit::EditArgs {
            path: self.resolve(&args.path),
            ..args
        };
        self.announce_write(&args.path);
        match edit::edit(&args, &self.files).await {
            Ok(out) => {
                let plural = if out.replaced == 1 { "" } else { "s" };
                let summary = format!(
                    "edited {} · {} change{plural}",
                    out.path.display(),
                    out.replaced
                );
                ToolOutcome::done(summary, format!("edited {}", out.path.display()))
            }
            Err(error) => ToolOutcome::failed(format!("edit failed: {error}"), error.to_string()),
        }
    }

    async fn call_bash(&self, arguments: &str) -> ToolOutcome {
        let args: bash::BashArgs = match self.parse("bash", arguments) {
            Ok(args) => args,
            Err(outcome) => return *outcome,
        };
        // 有总线才有面板：一次没人看得见的调用不必多出一格，而没有总线的工具集（测试里的那些）
        // 走的还是老路 —— 参数是 `None`，别的什么都没变。
        let watch = self.bus.as_ref().map(|bus| {
            let id = PaneId::fresh();
            // 订阅得在面板开出来之前拿到：这一格一出现，用户下一秒就可能按下 Ctrl+C，而总线上
            // 早于订阅的事件不会补发 —— 那一按就丢了。
            let stop = watch_for_stop(id, bus);
            self.open_tool_pane(id, bus, &args.command);
            bash::Watch {
                id,
                bus: bus.clone(),
                stop,
            }
        });
        // The pane is opened before the command is, so a call that never starts has to take its pane
        // back: the id is kept here because `bash` takes the watch by value.
        let pane = watch.as_ref().map(|watch| watch.id);
        match bash::bash(&args, &self.cwd, watch).await {
            Ok(out) => {
                let status = if out.interrupted {
                    "interrupted".to_string()
                } else if out.timed_out {
                    "timed out".to_string()
                } else if let Some(code) = out.exit_code {
                    format!("exit {code}")
                } else {
                    "no exit status".to_string()
                };
                let summary = format!(
                    "{} · {status} · {} lines",
                    first_line(&args.command),
                    out.text.total_lines
                );
                let content = out.render();
                if out.is_error() {
                    ToolOutcome::failed(summary, content)
                } else {
                    ToolOutcome::done(summary, content)
                }
            }
            Err(error) => {
                if let (Some(bus), Some(id)) = (self.bus.as_ref(), pane) {
                    self.close_tool_pane(id, bus);
                }
                ToolOutcome::failed(format!("bash failed: {error}"), error.to_string())
            }
        }
    }

    /// Take back the pane of a call that never started.
    ///
    /// The pane is opened before the command is, so a `bash` that fails outright — a working
    /// directory that is not there, a shell that will not spawn — leaves a pane with nothing to show
    /// and nothing that will ever appear in it. The tool result already says what went wrong; an
    /// empty pane would just be one more thing for the person to close by hand.
    fn close_tool_pane(&self, id: PaneId, bus: &EventBus) {
        self.panes.lock().retain(|held| *held != id);
        bus.publish(PaneEvent::Closed { id });
    }

    /// 把这次调用的那一格开出来，空间不够就先关掉最旧的一格。
    ///
    /// 开和关是一件事：一次调用只该多出一格，而布局是有上限的。关的是**最旧的**那一格 ——它
    /// 多半是上一次调用留下的，而关掉面板本身会把那次调用停下（面板没了，没人看得见它，也就
    /// 没人停得住它），所以被关掉的那一格不会是「没人管的进程」。
    fn open_tool_pane(&self, id: PaneId, bus: &EventBus, command: &str) {
        {
            let mut panes = self.panes.lock();
            // 先腾地方再开：中间那一瞬间布局也不会超过上限。
            while panes.len() >= TOOL_PANES {
                let Some(oldest) = panes.pop_front() else {
                    break;
                };
                bus.publish(PaneEvent::Closed { id: oldest });
            }
            panes.push_back(id);
        }
        bus.publish(PaneEvent::Opened {
            spec: PaneSpec::new(id, PaneKind::Terminal).with_title(first_line(command)),
        });
    }

    /// Parse a call's arguments, turning a malformed one into a result the model can act on.
    fn parse<T: DeserializeOwned>(
        &self,
        name: &str,
        arguments: &str,
    ) -> Result<T, Box<ToolOutcome>> {
        serde_json::from_str(arguments).map_err(|error| {
            Box::new(ToolOutcome::failed(
                format!("{name}: arguments could not be read"),
                format!(
                    "The arguments for `{name}` could not be read as JSON: {error}. They were: {arguments}"
                ),
            ))
        })
    }
}

/// 盯住总线上这一格的两个「停下」：面板里按了 Ctrl+C，或者面板被关掉了。
///
/// 这是引擎这一侧的活：面板只知道用户按了什么，而「这次调用要不要停」由起它的那一方决定 ——
/// 面板从来碰不到进程组。返回的收端交给 [`bash::Watch`]，被叫停时它会就绪。
///
/// 订阅要在面板开出来**之前**拿到（见 [`ToolSet::call_bash`]）：总线上早于订阅的事件不会补发，
/// 而用户可能在这一格刚出现时就按下键。
fn watch_for_stop(id: PaneId, bus: &EventBus) -> oneshot::Receiver<()> {
    let (mut tx, rx) = oneshot::channel();
    let mut events = bus.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // 收端没了就是这次调用已经结束了，谁也不用再停：任务收工。不然每跑一条命令都会
                // 留下一个永远在听的订阅者。
                _ = tx.closed() => return,
                event = events.recv() => match event {
                    Ok(Event::Pty(PtyEvent::Input { id: to, bytes }))
                        if to == id && bytes.contains(&CTRL_C) => break,
                    Ok(Event::Pty(PtyEvent::Kill { id: to })) if to == id => break,
                    // 这一格自己的输出、别的格子、别的事件，都和「停下」无关。
                    Ok(_) => {}
                    // 落后只说明中间的事件丢了；这次调用还在跑，就接着听。
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    // 总线没了：整个程序在收摊，没有谁会再叫停了。
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
        // 收端还在，说明这次调用还在等；收端已经没了，说明它结束了 —— 那就不用停了。
        let _ = tx.send(());
    });
    rx
}

/// The first line of a command, trimmed, for a one-line summary.
fn first_line(command: &str) -> String {
    let line = command.lines().next().unwrap_or("").trim();
    if line.chars().count() > 60 {
        let cut: String = line.chars().take(60).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-set-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_model_is_told_about_four_tools_each_with_its_own_parameters() {
        let set = ToolSet::new("/tmp");
        let specs = set.specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, ORDER, "the table's order is the ORDER constant");
        for spec in &specs {
            assert!(
                !spec.description.trim().is_empty(),
                "{} needs a description",
                spec.name
            );
            assert!(
                spec.parameters.get("properties").is_some(),
                "{} needs parameters the model can fill in: {}",
                spec.name,
                spec.parameters
            );
        }
    }

    #[test]
    fn every_description_names_the_parameters_its_schema_requires() {
        // The failure this guards against is the quiet one: a description that tells the model to
        // send `replacements` while the schema wants `edits` produces a call that is always wrong
        // and a tool that always refuses. The parameters the model must fill in are exactly the ones
        // the description has to mention.
        for spec in ToolSet::new("/tmp").specs() {
            let required = spec.parameters["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{} has no required list", spec.name));
            for name in required {
                let name = name.as_str().unwrap();
                assert!(
                    spec.description.contains(name),
                    "{}: the description never mentions `{name}`, which the schema requires. It says: {}",
                    spec.name,
                    spec.description
                );
            }
        }
    }

    #[tokio::test]
    async fn a_read_through_the_set_returns_the_file_and_a_one_line_summary() {
        let dir = scratch("read");
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let set = ToolSet::new(&dir);

        let outcome = set.call("read", r#"{"path":"a.txt"}"#).await;
        assert!(outcome.ok);
        assert_eq!(outcome.content, "one\ntwo");
        assert!(outcome.summary.contains("a.txt"), "{}", outcome.summary);
        assert!(outcome.summary.contains("2 lines"), "{}", outcome.summary);
    }

    #[tokio::test]
    async fn a_write_then_an_edit_go_through_the_same_file_queue() {
        let dir = scratch("write-edit");
        let set = ToolSet::new(&dir);

        let wrote = set
            .call("write", r#"{"path":"notes.md","content":"first\n"}"#)
            .await;
        assert!(
            wrote.ok && wrote.summary.starts_with("created"),
            "{wrote:?}"
        );

        let edited = set
            .call(
                "edit",
                r#"{"path":"notes.md","edits":[{"old_text":"first","new_text":"second"}]}"#,
            )
            .await;
        assert!(edited.ok, "{edited:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.md")).unwrap(),
            "second\n"
        );

        // Writing again says it replaced, which is the difference a human needs to see.
        let again = set
            .call("write", r#"{"path":"notes.md","content":"x"}"#)
            .await;
        assert!(again.summary.starts_with("replaced"), "{}", again.summary);
    }

    #[tokio::test]
    async fn an_edit_that_cannot_land_says_why_and_is_not_a_success() {
        let dir = scratch("bad-edit");
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        let set = ToolSet::new(&dir);

        let outcome = set
            .call(
                "edit",
                r#"{"path":"a.txt","edits":[{"old_text":"nowhere","new_text":"x"}]}"#,
            )
            .await;
        assert!(!outcome.ok, "{outcome:?}");
        // What the tool promises the model: which edit failed, and what to do about it.
        assert!(
            outcome.content.contains("edits[0]") && outcome.content.contains("Read the file again"),
            "{}",
            outcome.content
        );
        assert!(
            outcome.summary.starts_with("edit failed"),
            "the transcript line says what kind of thing happened: {}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn malformed_arguments_come_back_as_an_error_the_model_can_fix() {
        let set = ToolSet::new(scratch("bad-args"));
        let outcome = set.call("read", r#"{"path": 12}"#).await;
        assert!(!outcome.ok);
        assert!(
            outcome.content.contains("could not be read as JSON"),
            "{}",
            outcome.content
        );
        assert!(
            outcome.content.contains(r#"{"path": 12}"#),
            "the model is shown what it sent: {}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_tool_that_does_not_exist_lists_the_ones_that_do() {
        let set = ToolSet::new(scratch("unknown"));
        let outcome = set.call("grep", "{}").await;
        assert!(!outcome.ok);
        assert!(
            outcome.content.contains("read, write, edit, bash"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn bash_runs_in_the_session_directory_and_reports_its_status() {
        let dir = scratch("bash");
        let set = ToolSet::new(&dir);

        let ok = set.call("bash", r#"{"command":"pwd"}"#).await;
        assert!(ok.ok, "{ok:?}");
        assert!(
            ok.content.contains(&dir.display().to_string()),
            "{}",
            ok.content
        );
        assert!(ok.summary.contains("exit 0"), "{}", ok.summary);

        let bad = set.call("bash", r#"{"command":"exit 4"}"#).await;
        assert!(!bad.ok);
        assert!(bad.summary.contains("exit 4"), "{}", bad.summary);
    }

    #[tokio::test]
    async fn a_read_of_a_missing_file_is_a_result_not_a_panic() {
        let set = ToolSet::new(scratch("missing"));
        let outcome = set.call("read", r#"{"path":"nope.txt"}"#).await;
        assert!(!outcome.ok);
        assert!(!outcome.content.is_empty());
    }

    #[tokio::test]
    async fn a_write_announces_itself_so_the_watcher_can_stay_quiet() {
        let dir = scratch("announce");
        let bus = crate::event::EventBus::new(16);
        let mut events = bus.subscribe();
        let set = ToolSet::new(&dir).with_bus(bus);

        let target = dir.join("a.txt");
        let outcome = set
            .call(
                "write",
                &format!(
                    r#"{{"path":"{}","content":"hi"}}"#,
                    target.to_string_lossy()
                ),
            )
            .await;
        assert!(outcome.ok, "{}", outcome.content);

        // The announcement names the file, and it is on the bus: that is what lets the watcher
        // recognise the notification for this write and not report it back as somebody's change.
        match events.try_recv() {
            Ok(crate::event::Event::File(FileEvent::EditorWrote { path })) => {
                assert_eq!(path, target);
            }
            other => panic!("预期的公告，实际 {other:?}"),
        }

        // A read touches nothing, so it announces nothing: a read that did would cancel a real
        // change's notification and a pane would quietly miss it.
        let read = set
            .call(
                "read",
                &format!(r#"{{"path":"{}"}}"#, target.to_string_lossy()),
            )
            .await;
        assert!(read.ok, "{}", read.content);
        assert!(events.try_recv().is_err(), "读文件不写盘，不该有公告");
    }

    #[tokio::test]
    async fn a_call_that_never_starts_takes_its_pane_back() {
        let bus = EventBus::new(64);
        let mut events = bus.subscribe();
        let set = ToolSet::new(scratch("no-cwd")).with_bus(bus);

        // A working directory that is not there: `bash` fails before the command starts, so the pane
        // opened for it would have nothing in it — ever.
        let outcome = set
            .call(
                "bash",
                r#"{"command":"echo hi","cwd":"/definitely/not/here"}"#,
            )
            .await;
        assert!(!outcome.ok, "{}", outcome.content);

        let published = drained(&mut events);
        let opened: Vec<PaneId> = published
            .iter()
            .filter_map(|event| match event {
                Event::Pane(PaneEvent::Opened { spec }) => Some(spec.id),
                _ => None,
            })
            .collect();
        let closed: Vec<PaneId> = published
            .iter()
            .filter_map(|event| match event {
                Event::Pane(PaneEvent::Closed { id }) => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(opened.len(), 1, "先开了它");
        assert_eq!(closed, opened, "然后收了回去，不留一格空面板");
    }

    /// 总线上的事件，一次取完。
    fn drained(events: &mut broadcast::Receiver<Event>) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        seen
    }

    /// 等面板开出来，把它认出来。
    ///
    /// 等事件而不是猜 id：id 是引擎铸的，面板知道它的唯一途径就是这一条 `Opened`。
    async fn wait_for_pane(events: &mut broadcast::Receiver<Event>) -> PaneId {
        loop {
            match events.recv().await {
                Ok(Event::Pane(PaneEvent::Opened { spec })) => return spec.id,
                Ok(_) => {}
                Err(error) => panic!("面板一直没开出来：{error}"),
            }
        }
    }

    /// 等一个文件出现 —— 按 Ctrl+C 之前得等命令把自己的组号写下来。
    async fn wait_for_file(path: &Path) {
        for _ in 0..500 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{} 一直没出现", path.display());
    }

    /// 这一组还在不在。信号 0 只是问一问，什么都没发出去；`ESRCH` 就是「查无此组」。
    ///
    /// 和 `bash` 自己的测试问的是同一个问题：命令是引擎起的那一组里的，面板按下的那个键要
    /// 带走的是**一组**，不是 bash 一个。
    #[cfg(unix)]
    async fn group_gone(pgid: i32) -> bool {
        for _ in 0..100 {
            // SAFETY: 这里只查一个进程组在不在，没有内存可谈。
            let asked = unsafe { libc::kill(-pgid, 0) };
            if asked == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_bash_call_gets_a_terminal_pane_and_reports_what_it_saw() {
        let dir = scratch("bash-pane");
        let bus = crate::event::EventBus::new(64);
        let mut events = bus.subscribe();
        let set = ToolSet::new(&dir).with_bus(bus);

        let outcome = set.call("bash", r#"{"command":"echo hi"}"#).await;
        assert!(outcome.ok, "{outcome:?}");

        let seen = drained(&mut events);
        let id = match seen.first() {
            Some(Event::Pane(PaneEvent::Opened { spec })) => {
                assert_eq!(spec.kind, PaneKind::Terminal, "一格终端面板");
                assert_eq!(spec.title, "echo hi", "标题是这一格的用途：命令的首行");
                spec.id
            }
            other => panic!("面板该先开出来：{other:?}"),
        };
        // 引擎那边说的是同一件事：这一格在跑什么，写了什么，怎么结束的。
        assert!(
            seen.contains(&Event::Pty(PtyEvent::Started {
                id,
                title: "echo hi".to_string()
            })),
            "{seen:?}"
        );
        let output: Vec<u8> = seen
            .iter()
            .filter_map(|event| match event {
                Event::Pty(PtyEvent::Output { id: to, bytes }) if *to == id => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(String::from_utf8_lossy(&output), "hi\n");
        assert_eq!(
            seen.last(),
            Some(&Event::Pty(PtyEvent::Exited { id, code: Some(0) })),
            "{seen:?}"
        );
        // 一次调用只开一格，也没有顺手关掉谁：还没到上限。
        assert!(
            !seen
                .iter()
                .any(|event| matches!(event, Event::Pane(PaneEvent::Closed { .. }))),
            "{seen:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ctrl_c_in_the_pane_stops_the_command() {
        let dir = scratch("bash-ctrl-c");
        let bus = crate::event::EventBus::new(256);
        let mut events = bus.subscribe();
        let set = ToolSet::new(&dir).with_bus(bus.clone());
        let pid_file = dir.join("pid");
        let command = format!(
            r#"{{"command":"echo $$ > {}; sleep 30"}}"#,
            pid_file.display()
        );

        let press = async {
            let id = wait_for_pane(&mut events).await;
            // 等它把自己的组号写下来再按：不然杀的是一个还没写下 pid 的进程。
            wait_for_file(&pid_file).await;
            // 面板里的 Ctrl+C 就是一串字节，回到引擎是这一条事件 —— 面板从头到尾没碰过进程。
            bus.publish(PtyEvent::Input {
                id,
                bytes: vec![0x03],
            });
            id
        };
        let started = std::time::Instant::now();
        let (outcome, id) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(set.call("bash", &command), press)
        })
        .await
        .expect("按了 Ctrl+C 的调用该立刻回来，而不是等 sleep 30 跑完");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "叫停该马上生效，实际等了 {:?}",
            started.elapsed()
        );
        // 停的是这次调用，不是把它算成失败：答案照样回来，只是说清楚它是怎么结束的。
        assert!(outcome.ok, "被叫停不是调用失败：{outcome:?}");
        assert!(
            outcome.content.contains("interrupted"),
            "{}",
            outcome.content
        );
        assert!(
            outcome.summary.contains("interrupted"),
            "{}",
            outcome.summary
        );

        let pgid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            group_gone(pgid).await,
            "进程组 {pgid} 还活着：`sleep` 没跟着 bash 一起走"
        );

        // 面板也被告知结束了：它在等的那个东西没有了。
        assert!(
            drained(&mut events).contains(&Event::Pty(PtyEvent::Exited { id, code: None })),
            "面板该看到这一格结束了"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closing_the_pane_stops_the_command() {
        // 面板关掉也是「停下」：没人看得见的进程就是没人停得住的进程。
        let dir = scratch("bash-closed-pane");
        let bus = crate::event::EventBus::new(256);
        let mut events = bus.subscribe();
        let set = ToolSet::new(&dir).with_bus(bus.clone());

        let close = async {
            let id = wait_for_pane(&mut events).await;
            bus.publish(PtyEvent::Kill { id });
        };
        let started = std::time::Instant::now();
        let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(set.call("bash", r#"{"command":"sleep 30"}"#), close)
        })
        .await
        .expect("面板关掉之后这次调用该回来，而不是跑满 30 秒");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "关掉面板该马上生效，实际等了 {:?}",
            started.elapsed()
        );
        assert!(outcome.ok, "被叫停不是调用失败：{outcome:?}");
        assert!(
            outcome.content.contains("interrupted"),
            "{}",
            outcome.content
        );
    }

    #[tokio::test]
    async fn a_third_call_closes_the_oldest_pane() {
        let dir = scratch("bash-cap");
        let bus = crate::event::EventBus::new(256);
        let mut events = bus.subscribe();
        let set = ToolSet::new(&dir).with_bus(bus);

        for command in ["echo one", "echo two", "echo three"] {
            let outcome = set
                .call("bash", &format!(r#"{{"command":"{command}"}}"#))
                .await;
            assert!(outcome.ok, "{outcome:?}");
        }

        let mut opened = Vec::new();
        let mut closed = Vec::new();
        for event in drained(&mut events) {
            match event {
                Event::Pane(PaneEvent::Opened { spec }) => opened.push(spec.id),
                Event::Pane(PaneEvent::Closed { id }) => closed.push(id),
                _ => {}
            }
        }
        assert_eq!(opened.len(), 3, "三次调用，三次开面板");
        assert_eq!(closed, vec![opened[0]], "布局满了，关的是最旧的那一格");

        // 记得的是最近那两格：人回头看的，是这次和上一次。
        let panes = set.panes.lock();
        assert_eq!(
            panes.iter().copied().collect::<Vec<_>>(),
            vec![opened[1], opened[2]]
        );
    }

    #[tokio::test]
    async fn a_tool_set_without_a_bus_runs_bash_and_opens_no_panes() {
        let dir = scratch("bash-no-bus");
        let set = ToolSet::new(&dir);

        let outcome = set.call("bash", r#"{"command":"echo hi"}"#).await;

        assert!(outcome.ok, "{outcome:?}");
        assert!(outcome.content.contains("hi"), "{}", outcome.content);
        // 没有人能看的东西不必开出来：名单一直是空的。
        assert!(set.panes.lock().is_empty(), "没有总线就没有面板");
    }
}
