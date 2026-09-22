//! The `edit` tool: replace exact text, and say precisely why that could not be done.
//!
//! The model gives a snippet that occurs **once** in the file and what should stand there instead.
//! Everything that makes that safe to automate lives here rather than in the model's head:
//!
//! - **Every edit is matched against the file as it is, not against the file as previous edits
//!   left it.** Two edits in one call are two independent passages of the same original text, so
//!   their order in the request cannot change the result.
//! - **A snippet that is not unique is refused, not guessed.** Replacing the first of two matches
//!   is how a tool edits the wrong function.
//! - **Overlapping edits are refused.** They are a sign that the model meant one edit, not two.
//! - **The file's line endings and byte-order mark survive.** Matching happens against the LF form
//!   (so a model that has never seen the file's CRLF can still edit it), and the file's own style is
//!   put back on the way out.
//!
//! What this deliberately does *not* do yet is fuzzy matching — normalising curly quotes, trailing
//! whitespace and the like before giving up. Until then a near-miss is an error, and the error text
//! says what to do about it (read the file again, copy the snippet exactly, include more context),
//! which is a usable loop and an honest one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    queue::FileMutex,
    read::{ReadError, read_whole},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditArgs {
    pub path: String,
    /// One or more replacements. Each is matched against the original file.
    pub edits: Vec<Replace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replace {
    /// Text that occurs exactly once in the file.
    pub old_text: String,
    pub new_text: String,
}

impl EditArgs {
    /// The JSON schema the model is shown — beside the struct, so the two cannot drift.
    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to edit. `~` is expanded."
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Replacements, all matched against the file as it is now (not one after another). Keep `old_text` as small as it can be while still being unique.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": {
                                "type": "string",
                                "description": "Text to replace, copied from the file exactly, appearing only once."
                            },
                            "new_text": {
                                "type": "string",
                                "description": "What to put in its place. Empty deletes the snippet."
                            }
                        },
                        "required": ["old_text", "new_text"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }
}

#[derive(Debug)]
pub enum EditError {
    Read(ReadError),
    EmptyOldText { index: usize },
    NotFound { index: usize, path: PathBuf },
    NotUnique { index: usize, count: usize },
    Overlap { first: usize, second: usize },
    NoChange,
    Io { path: PathBuf, error: String },
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(f, "{error}"),
            // Every one of these is an instruction: the model reads it and has what it needs to
            // send a request that works.
            Self::EmptyOldText { index } => write!(
                f,
                "edits[{index}].old_text is empty. Give the exact text to replace — a snippet copied from the file."
            ),
            Self::NotFound { index, path } => write!(
                f,
                "edits[{index}]: could not find that text in {}. Read the file again and copy the snippet exactly, including its indentation.",
                path.display()
            ),
            Self::NotUnique { index, count } => write!(
                f,
                "edits[{index}]: old_text appears {count} times in the file. Include more of the surrounding lines so it matches once."
            ),
            Self::Overlap { first, second } => write!(
                f,
                "edits[{first}] and edits[{second}] cover overlapping parts of the file. Merge them into one edit, or make them cover different text."
            ),
            Self::NoChange => write!(
                f,
                "the edit would leave the file exactly as it is. Change new_text, or drop the edit."
            ),
            Self::Io { path, error } => {
                write!(f, "{} could not be written: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for EditError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutput {
    pub path: PathBuf,
    /// How many replacements were made.
    pub replaced: usize,
    /// Where the first one starts, for a pane that wants to jump there.
    pub first_changed_line: usize,
    pub bytes: usize,
}

/// Apply the edits to the file.
pub async fn edit(args: &EditArgs, files: &FileMutex) -> Result<EditOutput, EditError> {
    let path = crate::paths::expand_tilde(&args.path);
    let _turn = files.lock(&path).await;

    let original = read_whole(&path).map_err(EditError::Read)?;
    let (text, style) = normalize(&original);
    let plan = plan_edits(&text, &args.edits, &path)?;
    let edited = apply(&text, &plan.replacements);
    // `apply` works on the normalised text, so this is a comparison of the file's *content*: an
    // edit that only spells the file differently (LF against CRLF) is not an edit.
    if edited == text {
        return Err(EditError::NoChange);
    }
    let out = style.restore(edited);

    let bytes = out.len();
    let path_for_task = path.clone();
    tokio::task::spawn_blocking(move || {
        std::fs::write(&path_for_task, out.as_bytes()).map_err(|error| EditError::Io {
            path: path_for_task.clone(),
            error: error.to_string(),
        })
    })
    .await
    .map_err(|error| EditError::Io {
        path: path.clone(),
        error: format!("the edit task did not finish: {error}"),
    })??;

    Ok(EditOutput {
        path,
        replaced: plan.replacements.len(),
        first_changed_line: plan.first_changed_line,
        bytes,
    })
}

/// A byte range and what goes there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Replacement {
    start: usize,
    end: usize,
    new_text: String,
}

#[derive(Debug)]
struct Plan {
    replacements: Vec<Replacement>,
    first_changed_line: usize,
}

/// Find every edit's snippet, refuse the ambiguous and the overlapping ones, and sort what is left
/// so the replacements can be applied to the original text in one pass.
fn plan_edits(text: &str, edits: &[Replace], path: &Path) -> Result<Plan, EditError> {
    let mut found: Vec<Replacement> = Vec::with_capacity(edits.len());
    for (index, replace) in edits.iter().enumerate() {
        let (old, _) = normalize(&replace.old_text);
        if old.is_empty() {
            return Err(EditError::EmptyOldText { index });
        }
        let starts = find_all(text, &old);
        match starts.len() {
            0 => {
                return Err(EditError::NotFound {
                    index,
                    path: path.to_path_buf(),
                });
            }
            1 => found.push(Replacement {
                start: starts[0],
                end: starts[0] + old.len(),
                new_text: replace.new_text.clone(),
            }),
            count => return Err(EditError::NotUnique { index, count }),
        }
    }

    // Sorted by where they start, so "do these two touch?" is one comparison per neighbour.
    found.sort_by_key(|replacement| replacement.start);
    for pair in found.windows(2) {
        if pair[0].end > pair[1].start {
            // Which two, by their place in the request rather than their place in the file: that is
            // what the model can look at.
            let first = edits
                .iter()
                .position(|edit| normalize(&edit.old_text).0 == text[pair[0].start..pair[0].end])
                .unwrap_or(0);
            let second = edits
                .iter()
                .position(|edit| normalize(&edit.old_text).0 == text[pair[1].start..pair[1].end])
                .unwrap_or(1);
            return Err(EditError::Overlap { first, second });
        }
    }

    let first_changed_line = found
        .first()
        .map(|replacement| text[..replacement.start].matches('\n').count() + 1)
        .unwrap_or(1);
    Ok(Plan {
        replacements: found,
        first_changed_line,
    })
}

/// Build the edited text from `text` and sorted, non-overlapping replacements.
fn apply(text: &str, replacements: &[Replacement]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for replacement in replacements {
        out.push_str(&text[cursor..replacement.start]);
        out.push_str(&replacement.new_text);
        cursor = replacement.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Every occurrence of `needle` in `haystack`, counting the overlapping ones too.
///
/// Overlapping occurrences are the awkward case — `aa` occurs twice in `aaa` — and they count as
/// ambiguous: replacing "the first of them" is a guess, and a tool that edits files does not guess.
fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let at = from + offset;
        found.push(at);
        from = at + 1;
    }
    found
}

/// The file as it is matched, and what it takes to put its own style back.
struct Style {
    bom: bool,
    crlf: bool,
}

impl Style {
    /// Put the file's own line endings and BOM back on the edited text.
    ///
    /// A CRLF file is matched and edited in LF and written back in CRLF, which is what makes the
    /// untouched lines byte-identical: they were CRLF, they became LF for matching, and they are
    /// CRLF again here. The edit's own new lines get the file's endings too, so a file does not
    /// end up with two styles inside it.
    fn restore(&self, edited: String) -> String {
        let mut out = if self.crlf {
            edited.replace('\n', "\r\n")
        } else {
            edited
        };
        if self.bom {
            out.insert(0, '\u{feff}');
        }
        out
    }
}

/// Strip a byte-order mark and normalise line endings, so the model never has to know either.
fn normalize(text: &str) -> (String, Style) {
    let bom = text.starts_with('\u{feff}');
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let crlf = body.contains("\r\n");
    let normalized = if crlf {
        body.replace("\r\n", "\n")
    } else {
        body.to_string()
    };
    (normalized, Style { bom, crlf })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::write::{WriteArgs, write};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-edit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn file_with(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        write(
            &WriteArgs {
                path: path.to_str().unwrap().into(),
                content: content.into(),
            },
            &FileMutex::new(),
        )
        .await
        .unwrap();
        path
    }

    fn replace(old: &str, new: &str) -> Replace {
        Replace {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    async fn edit_file(path: &Path, edits: Vec<Replace>) -> Result<EditOutput, EditError> {
        edit(
            &EditArgs {
                path: path.to_str().unwrap().into(),
                edits,
            },
            &FileMutex::new(),
        )
        .await
    }

    #[tokio::test]
    async fn one_exact_snippet_is_replaced() {
        let dir = scratch("one");
        let path = file_with(&dir, "a.rs", "fn main() {\n    todo!()\n}\n").await;
        let out = edit_file(&path, vec![replace("todo!()", "println!(\"hi\")")])
            .await
            .unwrap();

        assert_eq!(out.replaced, 1);
        assert_eq!(out.first_changed_line, 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    println!(\"hi\")\n}\n"
        );
    }

    #[tokio::test]
    async fn every_edit_is_matched_against_the_original_file() {
        // The pair below is the test: if the edits were applied one after another, the second
        // would look for text the first one had already replaced.
        let dir = scratch("independent");
        let path = file_with(&dir, "b.rs", "let a = 1;\nlet b = 2;\n").await;
        let out = edit_file(
            &path,
            vec![
                replace("let a = 1;", "let a = 10;"),
                replace("let b = 2;", "let b = 20;"),
            ],
        )
        .await
        .unwrap();

        assert_eq!(out.replaced, 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "let a = 10;\nlet b = 20;\n"
        );
    }

    #[tokio::test]
    async fn a_snippet_that_is_not_there_says_to_read_the_file_again() {
        let dir = scratch("missing");
        let path = file_with(&dir, "c.rs", "fn main() {}\n").await;
        let error = edit_file(&path, vec![replace("fn other() {}", "x")])
            .await
            .unwrap_err();

        assert!(matches!(error, EditError::NotFound { index: 0, .. }));
        let text = error.to_string();
        assert!(text.contains("Read the file again"), "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn main() {}\n");
    }

    #[tokio::test]
    async fn a_snippet_that_appears_twice_is_refused_with_the_count() {
        let dir = scratch("ambiguous");
        let path = file_with(&dir, "d.rs", "let x = 1;\nlet x = 1;\n").await;
        let error = edit_file(&path, vec![replace("let x = 1;", "let x = 2;")])
            .await
            .unwrap_err();

        assert!(matches!(error, EditError::NotUnique { index: 0, count: 2 }));
        assert!(error.to_string().contains("appears 2 times"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "let x = 1;\nlet x = 1;\n",
            "a refused edit must not touch the file"
        );
    }

    #[tokio::test]
    async fn overlapping_occurrences_count_as_ambiguous() {
        // `aa` occurs twice in `aaa`, and "replace the first one" would be a guess.
        let dir = scratch("overlapping-occurrences");
        let path = file_with(&dir, "e.txt", "aaa").await;
        let error = edit_file(&path, vec![replace("aa", "b")])
            .await
            .unwrap_err();
        assert!(matches!(error, EditError::NotUnique { count: 2, .. }));
    }

    #[tokio::test]
    async fn edits_that_cover_the_same_text_are_refused_as_a_pair() {
        let dir = scratch("overlap");
        let path = file_with(&dir, "f.rs", "let value = 42;\n").await;
        let error = edit_file(
            &path,
            vec![
                replace("let value = 42;", "let value = 43;"),
                replace("value = 42", "value = 44"),
            ],
        )
        .await
        .unwrap_err();
        assert!(matches!(error, EditError::Overlap { .. }), "{error}");
        assert!(error.to_string().contains("overlapping"), "{error}");
    }

    #[tokio::test]
    async fn an_edit_that_changes_nothing_is_refused() {
        let dir = scratch("nochange");
        let path = file_with(&dir, "g.rs", "unchanged\n").await;
        let error = edit_file(&path, vec![replace("unchanged", "unchanged")])
            .await
            .unwrap_err();
        assert!(matches!(error, EditError::NoChange), "{error}");
    }

    #[tokio::test]
    async fn an_empty_old_text_is_refused() {
        let dir = scratch("empty");
        let path = file_with(&dir, "h.rs", "x\n").await;
        let error = edit_file(&path, vec![replace("", "y")]).await.unwrap_err();
        assert!(matches!(error, EditError::EmptyOldText { index: 0 }));
    }

    #[tokio::test]
    async fn a_crlf_file_is_matched_in_lf_and_written_back_in_crlf() {
        let dir = scratch("crlf");
        let path = file_with(&dir, "i.txt", "first\r\nsecond\r\nthird\r\n").await;
        let out = edit_file(&path, vec![replace("second", "SECOND")])
            .await
            .unwrap();

        assert_eq!(out.replaced, 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first\r\nSECOND\r\nthird\r\n",
            "the untouched lines keep their own bytes, and the file keeps its style"
        );
    }

    #[tokio::test]
    async fn a_snippet_written_with_lf_matches_a_crlf_file() {
        // The model has never seen the file's endings; asking it to reproduce them would be a
        // contract it cannot keep.
        let dir = scratch("crlf-model");
        let path = file_with(&dir, "j.txt", "one\r\ntwo\r\n").await;
        let out = edit_file(&path, vec![replace("one\ntwo", "1\n2")])
            .await
            .unwrap();
        assert_eq!(out.replaced, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\r\n2\r\n");
    }

    #[tokio::test]
    async fn a_byte_order_mark_survives_the_edit() {
        let dir = scratch("bom");
        let path = file_with(&dir, "k.txt", "\u{feff}hello\n").await;
        edit_file(&path, vec![replace("hello", "hi")])
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}hi\n");
    }

    #[tokio::test]
    async fn deleting_a_snippet_needs_only_an_empty_replacement() {
        let dir = scratch("delete");
        let path = file_with(&dir, "l.txt", "keep\ndrop me\nkeep\n").await;
        edit_file(&path, vec![replace("drop me\n", "")])
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\nkeep\n");
    }

    #[tokio::test]
    async fn the_snippet_is_matched_exactly_whitespace_and_all() {
        // Exact means exact. A snippet whose inner whitespace differs from the file's is a
        // not-found rather than a near-enough, and a snippet that includes the leading indentation
        // replaces it along with the text.
        let dir = scratch("indent");
        let path = file_with(&dir, "m.rs", "fn f() {\n    call();\n}\n").await;

        let error = edit_file(&path, vec![replace("call(  );", "other();")])
            .await
            .unwrap_err();
        assert!(matches!(error, EditError::NotFound { .. }), "{error}");

        edit_file(&path, vec![replace("    call();", "    other();")])
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn f() {\n    other();\n}\n"
        );
    }

    #[test]
    fn the_schema_and_the_struct_describe_the_same_parameters() {
        let schema = EditArgs::schema();
        let mut in_schema: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        in_schema.sort_unstable();
        let value = serde_json::to_value(EditArgs {
            path: "x".into(),
            edits: vec![replace("a", "b")],
        })
        .unwrap();
        let mut in_struct: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        in_struct.sort_unstable();
        assert_eq!(in_schema, in_struct);

        let inner = &schema["properties"]["edits"]["items"]["properties"];
        assert!(inner.get("old_text").is_some() && inner.get("new_text").is_some());
        assert_eq!(
            schema["properties"]["edits"]["items"]["required"],
            serde_json::json!(["old_text", "new_text"])
        );
    }
}
