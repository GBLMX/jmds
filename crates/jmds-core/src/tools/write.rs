//! The `write` tool: put a whole file there.
//!
//! The blunt one on purpose. `edit` is for changing part of a file; `write` is for creating one —
//! a prompt file the user asked for, a scratch file the agent keeps notes in, a generated listing.
//! It has no patch mode and no dry run: what it is given is what lands on disk.
//!
//! It takes the file's turn in [`super::queue`] while it works. A turn where the model asks for a
//! `write` and an `edit` of the same file produces both calls at once, and without the queue the
//! one that finishes second silently discards the other's work.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::queue::FileMutex;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteArgs {
    pub path: String,
    /// The file's whole content. Parent directories are created as needed.
    pub content: String,
}

impl WriteArgs {
    /// The JSON schema the model is shown — beside the struct, so the two cannot drift.
    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the file to write. `~` is expanded, and missing parent directories are created."
                },
                "content": {
                    "type": "string",
                    "description": "The file's entire content. This replaces the file — to change part of one, use edit."
                }
            },
            "required": ["path", "content"]
        })
    }
}

#[derive(Debug)]
pub enum WriteError {
    Io { path: PathBuf, error: String },
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, error } => {
                write!(f, "{} could not be written: {error}", path.display())
            }
        }
    }
}

impl std::error::Error for WriteError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutput {
    pub path: PathBuf,
    pub bytes: usize,
    /// Whether the file was there before. The transcript says "created" or "replaced", which is the
    /// difference between a new prompt file and someone's work being overwritten.
    pub created: bool,
}

/// Write the file, creating parent directories.
pub async fn write(args: &WriteArgs, files: &FileMutex) -> Result<WriteOutput, WriteError> {
    let path = crate::paths::expand_tilde(&args.path);
    let content = args.content.clone();

    // The lock is taken on the *given* path: `key_for` canonicalises it, so two spellings of the
    // same file meet here.
    let _turn = files.lock(&path).await;

    let path_for_task = path.clone();
    tokio::task::spawn_blocking(move || {
        let created = !path_for_task.exists();
        if let Some(dir) = path_for_task.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir).map_err(|error| WriteError::Io {
                path: dir.to_path_buf(),
                error: error.to_string(),
            })?;
        }
        std::fs::write(&path_for_task, content.as_bytes()).map_err(|error| WriteError::Io {
            path: path_for_task.clone(),
            error: error.to_string(),
        })?;
        Ok(WriteOutput {
            path: path_for_task,
            bytes: content.len(),
            created,
        })
    })
    .await
    .unwrap_or_else(|error| {
        Err(WriteError::Io {
            path,
            error: format!("the write task did not finish: {error}"),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-write-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_new_file_and_its_directories_appear() {
        let dir = scratch("new");
        let path = dir.join("deep/deeper/prompt.md");
        let args = WriteArgs {
            path: path.to_str().unwrap().into(),
            content: "# 提示词\n".into(),
        };
        let out = write(&args, &FileMutex::new()).await.unwrap();
        assert!(out.created);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# 提示词\n");
    }

    #[tokio::test]
    async fn writing_again_replaces_and_says_that_it_replaced() {
        let dir = scratch("replace");
        let path = dir.join("a.txt");
        let files = FileMutex::new();
        let first = WriteArgs {
            path: path.to_str().unwrap().into(),
            content: "one".into(),
        };
        assert!(write(&first, &files).await.unwrap().created);

        let second = WriteArgs {
            path: path.to_str().unwrap().into(),
            content: "two".into(),
        };
        let out = write(&second, &files).await.unwrap();
        assert!(!out.created, "the second write is a replacement");
        assert_eq!(out.bytes, 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
    }

    #[tokio::test]
    async fn a_write_to_a_directory_under_a_file_is_reported_not_panicked() {
        let dir = scratch("bad-path");
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        let args = WriteArgs {
            path: file.join("under-a-file.txt").to_str().unwrap().into(),
            content: "x".into(),
        };
        let error = write(&args, &FileMutex::new()).await.unwrap_err();
        assert!(
            error.to_string().contains("could not be written"),
            "{error}"
        );
    }

    #[test]
    fn the_schema_and_the_struct_describe_the_same_parameters() {
        let schema = WriteArgs::schema();
        let mut in_schema: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        in_schema.sort_unstable();
        let value = serde_json::to_value(WriteArgs {
            path: "x".into(),
            content: "y".into(),
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
        assert_eq!(schema["required"], serde_json::json!(["path", "content"]));
    }
}
