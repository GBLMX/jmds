//! The `read` tool: a file, in whole lines, and what to do when there is more of it.
//!
//! The contract, which the model is told about through [`ReadArgs::schema`]:
//!
//! - `path`, `offset` (1-based), `limit` (lines). No line numbers in the answer: the model steps
//!   through a long file with `offset`, and numbering every line would spend context on a column
//!   nothing needs.
//! - The answer is never cut mid-line and never silently short: it ends with the lines it showed
//!   and the offset that continues them, or — when a single line is longer than the whole budget —
//!   with the command that would read it instead.
//!
//! The size limits live in [`super::truncate`], because `bash` bounds its output the same way.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::truncate::{self, MAX_BYTES, MAX_LINES, Truncated};

/// What the model may pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadArgs {
    pub path: String,
    /// 1-based line to start at — what a previous answer's `offset` said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// How many lines to show at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl ReadArgs {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: None,
            limit: None,
        }
    }

    pub fn from_line(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// The JSON schema the model is shown.
    ///
    /// Written next to the struct so the two cannot drift, and a test round-trips them: a parameter
    /// the model is told about but that nothing reads is worse than no parameter, and so is one it
    /// is not told about but that changes the answer.
    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to read. `~` is expanded."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Line to start at, 1-based. Use the offset a previous read told you."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "How many lines to read at most."
                }
            },
            "required": ["path"]
        })
    }
}

#[derive(Debug)]
pub enum ReadError {
    NotFound(PathBuf),
    NotAFile(PathBuf),
    NotUtf8(PathBuf),
    Io { path: PathBuf, error: String },
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "{} does not exist", path.display()),
            Self::NotAFile(path) => write!(
                f,
                "{} is a directory; use bash (for example `ls {}`) to list it",
                path.display(),
                path.display()
            ),
            Self::NotUtf8(path) => write!(
                f,
                "{} is not text (invalid UTF-8); use bash to inspect it",
                path.display()
            ),
            Self::Io { path, error } => write!(f, "{} could not be read: {error}", path.display()),
        }
    }
}

impl std::error::Error for ReadError {}

/// A file as the model gets to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadOutput {
    pub path: PathBuf,
    /// The lines that were shown, and what was left out.
    pub shown: Truncated,
    /// The file's size, so a caller can log what a read cost.
    pub bytes: usize,
}

impl ReadOutput {
    /// The text the model is shown: the file, plus whatever it has to be told about the file.
    pub fn render(&self) -> String {
        if self.shown.first_line_overflows {
            let line = self.shown.first_line;
            return format!(
                "[{} line {line} is longer than {MAX_BYTES} bytes on its own, so it cannot be read \
                 line by line. Use bash to take a slice, for example: \
                 `sed -n '{line}p' {} | head -c {MAX_BYTES}`]",
                self.path.display(),
                self.path.display()
            );
        }

        let mut out = self.shown.text.clone();
        if let Some(offset) = self.shown.next_offset() {
            out.push_str(&format!(
                "\n\n[showing lines {}-{} of {}. Use offset={offset} to continue.]",
                self.shown.first_line, self.shown.last_line, self.shown.total_lines
            ));
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.shown.is_empty()
    }
}

/// Read a file.
///
/// The whole call is one blocking task: a stat, a read of at most a few tens of kilobytes, and a
/// UTF-8 check are not worth spreading over awaits, and the caller is an agent loop that has other
/// work to do while this runs.
pub async fn read(args: &ReadArgs) -> Result<ReadOutput, ReadError> {
    let path = crate::paths::expand_tilde(&args.path);
    let offset = args.offset.unwrap_or(1).max(1);
    let max_lines = args.limit.unwrap_or(MAX_LINES).max(1);

    tokio::task::spawn_blocking(move || read_blocking(path, offset, max_lines))
        .await
        .unwrap_or_else(|error| {
            Err(ReadError::Io {
                path: PathBuf::new(),
                error: format!("the read task did not finish: {error}"),
            })
        })
}

/// A file's whole text, with none of the limits of [`read`].
///
/// What a tool that rewrites a file needs: `read` shows the model a view, this is the thing itself.
/// It is the same three failures, reported the same way, so a caller does not have to decide which
/// of two read functions to call.
pub(crate) fn read_whole(path: &std::path::Path) -> Result<String, ReadError> {
    let metadata = std::fs::metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ReadError::NotFound(path.to_path_buf()),
        _ => ReadError::Io {
            path: path.to_path_buf(),
            error: error.to_string(),
        },
    })?;
    if metadata.is_dir() {
        return Err(ReadError::NotAFile(path.to_path_buf()));
    }
    let bytes = std::fs::read(path).map_err(|error| ReadError::Io {
        path: path.to_path_buf(),
        error: error.to_string(),
    })?;
    String::from_utf8(bytes).map_err(|_| ReadError::NotUtf8(path.to_path_buf()))
}

fn read_blocking(path: PathBuf, offset: usize, max_lines: usize) -> Result<ReadOutput, ReadError> {
    let bytes = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0) as usize;
    let text = read_whole(&path)?;
    let shown = truncate::head(&text, offset, max_lines, MAX_BYTES);
    Ok(ReadOutput { path, shown, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-read-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn numbered(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn a_small_file_comes_back_whole_and_without_ceremony() {
        let dir = scratch("small");
        let file = dir.join("a.txt");
        std::fs::write(&file, numbered(3)).unwrap();

        let out = read(&ReadArgs::new(file.to_str().unwrap())).await.unwrap();
        assert_eq!(out.render(), "line 1\nline 2\nline 3");
        assert!(out.shown.complete);
        assert_eq!(out.bytes, numbered(3).len());
    }

    #[tokio::test]
    async fn a_long_file_says_which_lines_these_are_and_how_to_continue() {
        let dir = scratch("long");
        let file = dir.join("b.txt");
        std::fs::write(&file, numbered(500)).unwrap();

        let out = read(&ReadArgs::new(file.to_str().unwrap()).with_limit(2))
            .await
            .unwrap();
        let text = out.render();
        assert!(text.starts_with("line 1\nline 2\n"), "{text}");
        assert!(
            text.contains("[showing lines 1-2 of 500. Use offset=3 to continue.]"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_offset_reads_on_from_where_the_last_answer_stopped() {
        let dir = scratch("offset");
        let file = dir.join("c.txt");
        std::fs::write(&file, numbered(10)).unwrap();

        let out = read(
            &ReadArgs::new(file.to_str().unwrap())
                .from_line(4)
                .with_limit(2),
        )
        .await
        .unwrap();
        assert!(out.render().starts_with("line 4\nline 5\n"));
        assert_eq!(out.shown.first_line, 4);
    }

    #[tokio::test]
    async fn reading_past_the_end_is_an_empty_answer_and_not_an_error() {
        let dir = scratch("past");
        let file = dir.join("d.txt");
        std::fs::write(&file, numbered(3)).unwrap();

        let out = read(&ReadArgs::new(file.to_str().unwrap()).from_line(99))
            .await
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(out.render(), "");
    }

    #[tokio::test]
    async fn a_single_line_longer_than_the_budget_gets_the_command_instead_of_a_fragment() {
        let dir = scratch("overflow");
        let file = dir.join("one-line.json");
        std::fs::write(
            &file,
            format!("{{\"blob\":\"{}\"}}", "x".repeat(MAX_BYTES + 10)),
        )
        .unwrap();

        let out = read(&ReadArgs::new(file.to_str().unwrap())).await.unwrap();
        let text = out.render();
        assert!(out.is_empty(), "{text}");
        assert!(text.contains("line 1 is longer than"), "{text}");
        assert!(
            text.contains("sed -n '1p'"),
            "the answer must be a way forward: {text}"
        );
    }

    #[tokio::test]
    async fn a_tilde_is_expanded_because_the_model_does_not_have_a_shell() {
        // Nothing about the read is special-cased for `~`; it goes through the same expansion the
        // editor will use, which is the point of having it in one place.
        let path = crate::paths::expand_tilde("~/jmds-does-not-exist");
        assert!(path.is_absolute(), "{}", path.display());
        assert!(!path.to_string_lossy().starts_with('~'));
    }

    #[tokio::test]
    async fn the_three_failures_a_model_can_cause_each_say_what_to_do_instead() {
        let dir = scratch("errors");
        let missing = dir.join("nope.txt");
        let error = read(&ReadArgs::new(missing.to_str().unwrap()))
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::NotFound(_)));
        assert!(error.to_string().contains("does not exist"));

        let error = read(&ReadArgs::new(dir.to_str().unwrap()))
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::NotAFile(_)));
        assert!(error.to_string().contains("bash"), "{error}");

        let binary = dir.join("binary.bin");
        std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let error = read(&ReadArgs::new(binary.to_str().unwrap()))
            .await
            .unwrap_err();
        assert!(matches!(error, ReadError::NotUtf8(_)));
        assert!(error.to_string().contains("bash"), "{error}");
    }

    #[test]
    fn the_schema_and_the_struct_describe_the_same_parameters() {
        // The failure this guards against is a parameter that exists in one and not the other:
        // the model would either be told about something nothing reads, or be kept from a
        // parameter that changes the answer.
        let schema = ReadArgs::schema();
        let properties = schema["properties"].as_object().expect("properties");
        let mut in_schema: Vec<&str> = properties.keys().map(String::as_str).collect();
        in_schema.sort_unstable();

        let args = ReadArgs::new("x").from_line(2).with_limit(3);
        let serialized = serde_json::to_value(&args).unwrap();
        let mut in_struct: Vec<&str> = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        in_struct.sort_unstable();

        assert_eq!(in_schema, in_struct);
        assert_eq!(schema["required"], serde_json::json!(["path"]));
        // And the arguments the model sends parse back into the struct it describes.
        let parsed: ReadArgs = serde_json::from_value(serialized).unwrap();
        assert_eq!(parsed, args);
    }
}
