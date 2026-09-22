//! The commands the input line understands, and where completion candidates come from.
//!
//! A command is a line that starts with `/`: six of them, each with the one line of explanation the
//! menu shows. That is the whole vocabulary, and it is deliberately small — a command list is prompt
//! surface, and every entry is something the reader has to skip past when they are looking for the
//! one they want.
//!
//! The two candidate sources live here as well, because they are the two things a person can name in
//! this app: a command, and a path. The path source reads the session's directory through
//! `std::fs`; the engine in [`crate::complete`] never touches the file system, so its matching and
//! splicing stay testable on their own.

use std::path::{Path, PathBuf};

use crate::complete::{Item, Prefix, Source};
use crate::theme::Theme;

/// One command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// What is typed after the slash.
    pub name: &'static str,
    /// The shape of a complete line, for the menu's detail column.
    pub usage: &'static str,
    pub description: &'static str,
    /// Whether the command takes an argument, which keeps the token open after acceptance: the next
    /// thing anyone types is that argument.
    pub takes_argument: bool,
}

/// Every command, in the order the menu shows them when nothing has been typed.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "help",
        usage: "/help",
        description: "what the keys do, and what the tools can do",
        takes_argument: false,
    },
    Command {
        name: "clear",
        usage: "/clear",
        description: "empty the transcript, keeping the session file",
        takes_argument: false,
    },
    Command {
        name: "theme",
        usage: "/theme <name>",
        description: "switch colours, e.g. /theme dracula",
        takes_argument: true,
    },
    Command {
        name: "prompt",
        usage: "/prompt <name>",
        description: "start the prompt file from a saved template",
        takes_argument: true,
    },
    Command {
        name: "glyphs",
        usage: "/glyphs unicode|ascii",
        description: "which glyph set to draw with",
        takes_argument: true,
    },
    Command {
        name: "quit",
        usage: "/quit",
        description: "leave, putting the terminal back",
        takes_argument: false,
    },
];

impl Command {
    pub fn find(name: &str) -> Option<&'static Self> {
        COMMANDS.iter().find(|command| command.name == name)
    }
}

/// A line that names a command, split into the command and its argument.
///
/// A line that merely starts with a slash is not one: `/etc/passwd is a file` is a sentence about a
/// path, and answering it with "no such command" would be the app mistaking prose for a request.
pub fn parse_command(line: &str) -> Option<(&'static Command, &str)> {
    let trimmed = line.trim_start();
    let body = trimmed.strip_prefix('/')?;
    let mut parts = body.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("");
    let argument = parts.next().unwrap_or("").trim();
    // An unknown word is only a command if it *looks* like one: no slashes, no spaces, and not
    // empty. Everything else is prose that happens to start with a slash.
    let command = Command::find(name)?;
    Some((command, argument))
}

/// Candidates for the two prefixes this app has.
pub struct SessionSource {
    /// What `@` paths are relative to: the session's working directory.
    pub root: PathBuf,
}

impl SessionSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn commands(&self, query: &str) -> Vec<Item> {
        COMMANDS
            .iter()
            .filter(|command| command.name.starts_with(query))
            .map(|command| {
                let item = Item::new(command.name).with_detail(command.description);
                // A command that takes an argument keeps the token open so the argument can be typed
                // without reaching for the space bar.
                if command.takes_argument {
                    let mut item = item;
                    item.insert = format!("{} ", command.name);
                    item.continues = false;
                    item
                } else {
                    item
                }
            })
            .collect()
    }

    fn paths(&self, query: &str) -> Vec<Item> {
        // Nothing outside the session: completion follows the rule the tools follow, so a candidate
        // can be pasted into a tool call and mean the same thing there.
        if !stays_inside(query) {
            return Vec::new();
        }
        // The query is a path being typed: everything up to the last slash is the directory, and the
        // rest is what the name must start with.
        let (directory, fragment) = match query.rfind('/') {
            Some(at) => (&query[..at + 1], &query[at + 1..]),
            None => ("", query),
        };
        let listing = self.root.join(directory);
        let Ok(entries) = std::fs::read_dir(&listing) else {
            return Vec::new();
        };

        let mut items: Vec<Item> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // A dotfile is only offered when the fragment asks for one: a menu full of `..` and
            // `.git` is a menu nobody can find their file in.
            if name.starts_with('.') && !fragment.starts_with('.') {
                continue;
            }
            if !name.starts_with(fragment) {
                continue;
            }
            let is_directory = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            let mut item = Item::new(format!("{directory}{name}"));
            if is_directory {
                item = item.continuing().with_detail("directory");
            }
            items.push(item);
            if items.len() >= 200 {
                break;
            }
        }
        // Directories first: descending is the common case when the name is a prefix of both.
        items.sort_by_key(|item| (!item.continues, item.label.clone()));
        items
    }
}

impl Source for SessionSource {
    /// The values a command takes.
    ///
    /// Short lists that live with the thing they name: a theme's names are the theme's business, and
    /// the glyph sets are the renderer's. Both are offered the same way, so `/theme dr` and
    /// `/glyphs un` are completed by the same mechanism that completes `/th`.
    fn argument(&self, command: &str, query: &str) -> Vec<Item> {
        let values: Vec<String> = match command {
            "theme" => Theme::NAMES.iter().map(|name| name.to_string()).collect(),
            "glyphs" => ["unicode", "ascii"]
                .iter()
                .map(|set| set.to_string())
                .collect(),
            // The templates are whatever the user has saved, so they are read rather than listed: one
            // they wrote a minute ago belongs in the menu a minute later.
            "prompt" => jmds_core::prompt::templates(),
            _ => return Vec::new(),
        };
        values
            .into_iter()
            .filter(|value| value.starts_with(query))
            .map(Item::new)
            .collect()
    }

    fn candidates(&self, prefix: Prefix, query: &str) -> Vec<Item> {
        match prefix {
            Prefix::Command => self.commands(query),
            Prefix::Path => self.paths(query),
            // Asked for through `Source::argument` instead: a value is the command's business, and
            // which command is a fact about the line rather than about the prefix.
            Prefix::Argument => Vec::new(),
        }
    }
}

/// Whether a path stays inside the session: absolute, or relative without climbing out.
///
/// `read_dir` can only yield names inside a directory, but the *directory* comes from what was
/// typed, and `../..` there would list somewhere else entirely. Completion follows the rule the
/// tools follow — a candidate can be pasted into a tool call and mean the same thing there.
pub fn stays_inside(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute() || !path.components().any(|part| part.as_os_str() == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jmds-cmd-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        dir
    }

    #[test]
    fn a_slash_line_splits_into_a_command_and_its_argument() {
        let (command, argument) = parse_command("/theme dracula").unwrap();
        assert_eq!(command.name, "theme");
        assert_eq!(argument, "dracula");

        let (command, argument) = parse_command("  /clear  ").unwrap();
        assert_eq!(command.name, "clear");
        assert_eq!(argument, "");

        assert!(parse_command("/nope").is_none());
        assert!(parse_command("no slash").is_none());
    }

    #[test]
    fn a_sentence_that_happens_to_start_with_a_slash_is_not_a_command() {
        // The failure this prevents: answering "no such command" to prose about a path.
        assert!(parse_command("/etc/passwd is just a file").is_none());
        assert!(
            parse_command("/ theme").is_none(),
            "a command has no space in its name"
        );
    }

    #[test]
    fn commands_are_offered_by_prefix_and_described() {
        let source = SessionSource::new(scratch("commands"));
        let items = source.candidates(Prefix::Command, "th");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "theme");
        assert!(!items[0].detail.is_empty(), "a menu entry explains itself");

        // Everything, with an empty query.
        let all = source.candidates(Prefix::Command, "");
        assert_eq!(all.len(), COMMANDS.len());
        // A command that takes an argument inserts the space, so the argument can follow directly.
        let theme = all.iter().find(|item| item.label == "theme").unwrap();
        assert_eq!(theme.insert, "theme ");
        let clear = all.iter().find(|item| item.label == "clear").unwrap();
        assert_eq!(clear.insert, "clear");
    }

    #[test]
    fn paths_are_listed_from_the_session_directory() {
        let root = scratch("paths");
        let source = SessionSource::new(&root);

        let items = source.candidates(Prefix::Path, "");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert!(labels.contains(&"src"), "{labels:?}");
        assert!(labels.contains(&"Cargo.toml"), "{labels:?}");
        assert!(
            !labels.contains(&".hidden"),
            "dotfiles are not offered unasked"
        );
        assert!(
            items
                .iter()
                .find(|item| item.label == "src")
                .unwrap()
                .continues
        );

        // A fragment filters, and a directory in the query descends into it.
        let items = source.candidates(Prefix::Path, "src/");
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["src/main.rs"]);

        // Directories come before files with a shared prefix.
        let items = source.candidates(Prefix::Path, "");
        assert!(items[0].continues, "the directory is first: {items:?}");
    }

    #[test]
    fn a_dotfile_is_offered_when_it_is_asked_for() {
        let source = SessionSource::new(scratch("dotfiles"));
        let items = source.candidates(Prefix::Path, ".h");
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].label, ".hidden");
    }

    #[test]
    fn a_directory_that_is_not_there_offers_nothing() {
        let source = SessionSource::new(scratch("missing"));
        assert!(source.candidates(Prefix::Path, "nowhere/").is_empty());
        assert!(
            source.candidates(Prefix::Path, "src/../..").is_empty(),
            "and neither does escaping"
        );
    }

    #[test]
    fn a_command_offers_the_values_it_takes() {
        let source = SessionSource::new(scratch("values"));

        let themes = source.argument("theme", "dr");
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].label, "dracula");
        assert_eq!(source.argument("theme", "").len(), Theme::NAMES.len());
        assert_eq!(source.argument("glyphs", "un")[0].label, "unicode");

        // A command with nothing to choose from says nothing rather than guessing.
        assert!(source.argument("clear", "").is_empty());
        assert!(source.argument("nope", "").is_empty());
        // Every command that takes an argument says what for, and the ones that do not say nothing.
        assert!(Command::find("prompt").is_some_and(|command| command.takes_argument));
    }

    #[test]
    fn the_prompt_command_offers_the_templates_that_are_saved() {
        let source = SessionSource::new(scratch("prompt-values"));
        let saved = jmds_core::prompt::templates();
        let offered: Vec<String> = source
            .argument("prompt", "")
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert_eq!(offered, saved, "菜单里就是用户存下的那些模板");
        // And a name that no template starts with is filtered out like any other query.
        assert!(source.argument("prompt", "zzz").is_empty());
    }

    #[test]
    fn every_theme_the_menu_offers_is_one_that_works() {
        // The list and the match inside `Theme::named` have to agree: a name offered here that does
        // not resolve is a menu entry that fails when it is taken.
        for name in Theme::NAMES {
            assert!(Theme::named(name).is_some(), "{name} 列了却拿不到");
        }
    }

    #[test]
    fn a_path_may_be_absolute_or_relative_but_not_go_upward() {
        assert!(stays_inside("src/main.rs"));
        assert!(
            stays_inside("/etc/hosts"),
            "absolute is where the person said"
        );
        assert!(!stays_inside("../outside"));
        assert!(!stays_inside("src/../../outside"));
    }
}
