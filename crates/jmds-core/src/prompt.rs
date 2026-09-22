//! The prompt file: what the human is asking for, written down before it is sent.
//!
//! The shape is deliberate and small. A file that says *which file* and *what to do to it*, plus
//! whatever else is worth writing, is the unit of work this app is built around — it survives a
//! session ending, it can be edited while the answer arrives, and it is the thing a picker will
//! list later.
//!
//! ```text
//! ---
//! path: crates/jmds-core/src/tools/bash.rs
//! instruction: 让默认超时变成 120 秒
//! ---
//! 顺便看一眼 clamp 的提示语是不是也该跟着改。
//! ```
//!
//! Three decisions worth naming:
//!
//! - **The frontmatter is two keys, parsed by hand.** No YAML: two `key: value` lines under a pair
//!   of `---` fences is a format nobody has to learn, and a parser for it is twenty lines rather
//!   than a second dependency with its own opinions about quotes, anchors and dates.
//! - **A file with no frontmatter is still a prompt.** The whole text becomes the instruction's
//!   body: refusing to send someone's prose because it lacks a header would make the format a tax
//!   rather than a convenience.
//! - **Assembly is where the path and the instruction meet the body, and it is pure.** What gets
//!   sent is a function of the file, so it can be shown, tested and reasoned about before anything
//!   is sent — which is the point of writing it down first.

use std::path::{Path, PathBuf};

/// The keys the frontmatter understands.
const PATH_KEY: &str = "path";
const INSTRUCTION_KEY: &str = "instruction";

/// A prompt file, parsed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Prompt {
    /// The file the work is about, as the human wrote it.
    pub path: Option<String>,
    /// What to do, in one line.
    pub instruction: Option<String>,
    /// Everything after the frontmatter, verbatim.
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// A fence that was opened and never closed.
    UnclosedFrontmatter,
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnclosedFrontmatter => write!(
                f,
                "开头有 `---` 却没有收尾的 `---`：要么补上，要么把那行删掉，正文就当提示词发出去"
            ),
        }
    }
}

impl std::error::Error for PromptError {}

impl Prompt {
    /// Read one. See the module docs for the format.
    pub fn parse(text: &str) -> Result<Self, PromptError> {
        let mut lines = text.lines();
        // The fence has to be the very first thing: a `---` in the middle of a paragraph is a
        // horizontal rule someone meant, not a header.
        if lines.next().map(str::trim_end) != Some("---") {
            // Trailing blank lines are not content: a file saved by an editor ends with a newline,
            // and sending that newline is sending a stray character nobody typed.
            return Ok(Self {
                body: text.trim().to_string(),
                ..Self::default()
            });
        }

        let mut prompt = Self::default();
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                closed = true;
                break;
            }
            let trimmed = line.trim();
            if let Some(value) = trimmed.strip_prefix(PATH_KEY).and_then(strip_colon) {
                prompt.path = Some(value.to_string()).filter(|value| !value.is_empty());
            } else if let Some(value) = trimmed.strip_prefix(INSTRUCTION_KEY).and_then(strip_colon)
            {
                prompt.instruction = Some(value.to_string()).filter(|value| !value.is_empty());
            }
            // Anything else in the header is a key this build does not know. It is skipped rather
            // than refused: a file written for a later version is still worth sending.
        }
        if !closed {
            return Err(PromptError::UnclosedFrontmatter);
        }
        prompt.body = lines.collect::<Vec<&str>>().join("\n").trim().to_string();
        Ok(prompt)
    }

    /// What the model is sent.
    ///
    /// The pieces are labelled because the model has to know which is which: an instruction and a
    /// path that arrive as loose prose are one more thing for it to guess about. The file's path is
    /// resolved against `base` when it is relative, so the path the model sees is one it can paste
    /// into a tool call.
    pub fn assemble(&self, base: &Path) -> String {
        let mut out = String::new();
        if let Some(path) = &self.path {
            let resolved = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                base.join(path)
            };
            out.push_str(&format!("File: {}\n", resolved.display()));
        }
        if let Some(instruction) = &self.instruction {
            out.push_str(&format!("Task: {instruction}\n"));
        }
        if !self.body.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&self.body);
        }
        out.trim_end().to_string()
    }

    /// Whether there is anything to send.
    pub fn is_empty(&self) -> bool {
        self.path.is_none() && self.instruction.is_none() && self.body.trim().is_empty()
    }

    /// The file a first run finds, so the format is learned by seeing it rather than by reading
    /// about it.
    ///
    /// Every line of guidance is a comment *inside* the header, and there is no body: an untouched
    /// template has to assemble to nothing, or the first thing a new user could send is the
    /// documentation. Comments are skipped by the parser for the same reason — the header is where
    /// `#` means "note to self", and the body is exactly what will be sent.
    pub fn template() -> String {
        format!(
            "---\n\
             # 要改哪个文件（相对路径按会话目录解析；不写就只发正文）\n\
             {PATH_KEY}: \n\
             # 一句话说清要做什么\n\
             {INSTRUCTION_KEY}: \n\
             # 正文写在下面那条 --- 之后：复现步骤、约束、要保留的行为、你不想再看到的东西。\n\
             # 发出去的正文里 `#` 也算正文，所以注释只能写在这一段里。\n\
             ---\n"
        )
    }
}

/// `path: value` → `Some("value")`; `pathy` → `None`, so a key is only matched when it is the key.
fn strip_colon(rest: &str) -> Option<&str> {
    let value = rest.strip_prefix(':')?;
    Some(value.trim())
}

/// Where prompt files live. Beside the sessions, under the config directory: they are the user's
/// own writing, not something reproducible.
pub fn prompts_dir() -> PathBuf {
    crate::paths::config_dir().join("prompts")
}

/// The templates in `dir`, by name: `<name>.md` is the template `name`.
///
/// A directory that is not there yet is a user who has never saved one, not an error; and a name that
/// does not end in `.md` is not a template, because everything in here is prompt text by definition.
pub fn templates_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            // A file and not a directory that happens to be called `x.md`: what is offered in a menu
            // has to be something that can be read.
            (path.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("md"))
            .then(|| path.file_stem()?.to_str().map(str::to_string))
            .flatten()
        })
        .collect();
    names.sort();
    names
}

/// The templates the user has saved.
pub fn templates() -> Vec<String> {
    templates_in(&prompts_dir())
}

/// The file the app opens when nothing else is asked for.
pub fn default_prompt_path() -> PathBuf {
    prompts_dir().join("prompt.md")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-prompt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn templates_are_the_markdown_files_by_name() {
        let dir = scratch("templates");
        std::fs::write(dir.join("review.md"), "---\n---\n").unwrap();
        std::fs::write(dir.join("fix.md"), "---\n---\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "not a template").unwrap();
        std::fs::create_dir_all(dir.join("nested.md")).unwrap();

        assert_eq!(templates_in(&dir), ["fix", "review"], "按名字排，只有 .md");
    }

    #[test]
    fn a_prompts_directory_that_is_not_there_has_no_templates() {
        // Never saved one, or never run the app: an empty menu, not an error.
        assert!(templates_in(&scratch("missing").join("nowhere")).is_empty());
    }

    use super::*;

    #[test]
    fn a_header_with_both_keys_parses_and_assembles_in_order() {
        let text =
            "---\npath: src/main.rs\ninstruction: 加一个 --version\n---\n\n顺便看看帮助文本。\n";
        let prompt = Prompt::parse(text).unwrap();
        assert_eq!(prompt.path.as_deref(), Some("src/main.rs"));
        assert_eq!(prompt.instruction.as_deref(), Some("加一个 --version"));
        assert_eq!(prompt.body, "顺便看看帮助文本。");

        let assembled = prompt.assemble(Path::new("/work"));
        // The expected text is built the same way the prompt is: a path joined onto a directory is
        // spelled however the platform spells it, and asserting a `/` would be asserting Unix.
        assert_eq!(
            assembled,
            format!(
                "File: {}\nTask: 加一个 --version\n\n顺便看看帮助文本。",
                Path::new("/work").join("src/main.rs").display()
            )
        );
    }

    #[test]
    fn an_absolute_path_is_left_alone() {
        let prompt = Prompt::parse("---\npath: /etc/hosts\n---\n").unwrap();
        assert!(
            prompt
                .assemble(Path::new("/work"))
                .starts_with("File: /etc/hosts")
        );
    }

    #[test]
    fn a_file_with_no_header_is_all_body() {
        // Refusing someone's prose because it lacks a header would make the format a tax.
        let prompt = Prompt::parse("把 read 的超时改成 120 秒\n和 clamp 的提示语一起看\n").unwrap();
        assert!(prompt.path.is_none() && prompt.instruction.is_none());
        assert_eq!(
            prompt.body,
            "把 read 的超时改成 120 秒\n和 clamp 的提示语一起看"
        );
        assert_eq!(prompt.assemble(Path::new("/work")), prompt.body);
    }

    #[test]
    fn a_open_fence_is_an_error_rather_than_a_guess() {
        let error = Prompt::parse("---\npath: x\n").unwrap_err();
        assert_eq!(error, PromptError::UnclosedFrontmatter);
        assert!(error.to_string().contains("收尾"), "{error}");
    }

    #[test]
    fn unknown_keys_and_empty_values_are_tolerated() {
        let text =
            "---\nmodel: deepseek-reasoner\npath:\ninstruction: 做点事\nlater: yes\n---\nbody\n";
        let prompt = Prompt::parse(text).unwrap();
        assert_eq!(prompt.path, None, "an empty value is not a path");
        assert_eq!(prompt.instruction.as_deref(), Some("做点事"));
        assert_eq!(prompt.body, "body");
    }

    #[test]
    fn a_horizontal_rule_later_in_the_text_is_not_a_header() {
        // Only the first line opens the header. A `---` under a paragraph is a rule the human drew.
        let prompt = Prompt::parse("先说一句\n\n---\n\n后面还有\n").unwrap();
        assert_eq!(prompt.body, "先说一句\n\n---\n\n后面还有");
    }

    #[test]
    fn a_key_that_merely_starts_with_a_key_name_is_not_that_key() {
        let prompt = Prompt::parse("---\npathology: x\ninstructional: y\n---\n").unwrap();
        assert_eq!(prompt.path, None);
        assert_eq!(prompt.instruction, None);
    }

    #[test]
    fn the_template_round_trips_through_the_parser() {
        // The file a first run writes has to be a file the parser accepts, or the first run is
        // broken for everyone.
        let prompt = Prompt::parse(&Prompt::template()).expect("the template parses");
        assert!(prompt.path.is_none(), "an empty value means no path");
        assert!(prompt.instruction.is_none());
        assert!(prompt.is_empty());
    }

    #[test]
    fn an_empty_prompt_is_empty_by_every_measure() {
        assert!(Prompt::parse("").unwrap().is_empty());
        assert!(Prompt::parse("---\n---\n").unwrap().is_empty());
        assert!(!Prompt::parse("x").unwrap().is_empty());
    }

    #[test]
    fn the_prompt_directory_sits_beside_the_sessions() {
        let dir = prompts_dir();
        assert!(dir.ends_with("prompts"), "{}", dir.display());
        assert_eq!(default_prompt_path().file_name().unwrap(), "prompt.md");
    }
}
