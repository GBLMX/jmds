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

use std::path::{Path, PathBuf};

use jmds_api::ToolSpec;
use serde::de::DeserializeOwned;

use super::{bash, edit, queue::FileMutex, read, write};

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
        match bash::bash(&args, &self.cwd).await {
            Ok(out) => {
                let status = if out.timed_out {
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
            Err(error) => ToolOutcome::failed(format!("bash failed: {error}"), error.to_string()),
        }
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
}
