//! What the command line can ask for.
//!
//! Hand-parsed, because three flags and a value do not need a parser crate, and the failure mode
//! that matters — a `--resume` that cannot find its session — has to be an error either way. What
//! this module refuses to do is guess: an unknown flag is an error rather than a session named
//! after it, because silently starting a new conversation when someone asked to continue an old one
//! is worse than not starting at all.

/// What the process was asked to start with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// A new conversation.
    New,
    /// Continue the most recent conversation held in this directory.
    Continue,
    /// Continue a named session.
    Resume(String),
    /// Continue a session — the latest here by default — in a new file, keeping the first `keep`
    /// messages of its history.
    Branch {
        from: Option<String>,
        keep: Option<usize>,
    },
    /// Print how to use this and stop.
    Help,
}

pub const USAGE: &str = "\
jmds — a coding assistant in the terminal

usage:
  jmds                    start a new conversation
  jmds --continue         continue the latest conversation in this directory
  jmds --resume <id>      continue a named conversation
  jmds <id>               the same thing, for a person who has the id in hand
  jmds --branch [<id>]    start a new conversation branched off one (the latest here by default)
  jmds --keep <n>         with --branch: how many messages of its history to keep
  jmds --help             this";

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub start: Start,
    /// Only meaningful with [`Start::Branch`].
    pub keep: Option<usize>,
}

/// Parse the arguments *after* the program's name.
///
/// A bare word is a session id: it is what someone types when they have an id in hand, and refusing
/// it would mean making them remember which flag it goes with.
pub fn parse(arguments: &[String]) -> Result<Args, String> {
    let mut start = Start::New;
    let mut keep = None;
    let mut branched = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        match argument {
            "--help" | "-h" => {
                return Ok(Args {
                    start: Start::Help,
                    keep: None,
                });
            }
            "--continue" | "-c" => start = Start::Continue,
            "--resume" | "-r" => {
                index += 1;
                let id = arguments
                    .get(index)
                    .ok_or_else(|| "--resume 需要会话 id".to_string())?;
                start = Start::Resume(id.clone());
            }
            "--branch" | "-b" => {
                branched = true;
                // The id is optional, so the next argument is only taken when it is not a flag.
                let named = arguments
                    .get(index + 1)
                    .filter(|next| !next.starts_with('-'));
                let from = named.cloned();
                if named.is_some() {
                    index += 1;
                }
                start = Start::Branch { from, keep: None };
            }
            "--keep" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--keep 需要行数".to_string())?;
                let count = value
                    .parse::<usize>()
                    .map_err(|_| format!("--keep 要的是一个数字，拿到的是 {value}"))?;
                keep = Some(count);
            }
            other if other.starts_with('-') => return Err(format!("不认识的参数：{other}")),
            id => start = Start::Resume(id.to_string()),
        }
        index += 1;
    }

    if keep.is_some() && !branched {
        return Err("--keep 只在 --branch 时有意义".to_string());
    }
    Ok(Args { start, keep })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|argument| argument.to_string()).collect()
    }

    fn start(list: &[&str]) -> Start {
        parse(&args(list)).expect("能解析").start
    }

    #[test]
    fn no_arguments_starts_a_new_conversation() {
        assert_eq!(start(&[]), Start::New);
    }

    #[test]
    fn continuing_takes_the_latest_and_resuming_takes_a_name() {
        assert_eq!(start(&["--continue"]), Start::Continue);
        assert_eq!(start(&["--resume", "100-0"]), Start::Resume("100-0".into()));
        // A bare word is an id: that is what someone with an id in hand types.
        assert_eq!(start(&["100-0"]), Start::Resume("100-0".into()));
    }

    #[test]
    fn branching_defaults_to_the_latest_here_and_can_be_given_a_name() {
        assert_eq!(
            start(&["--branch"]),
            Start::Branch {
                from: None,
                keep: None
            }
        );
        assert_eq!(
            start(&["--branch", "100-0"]),
            Start::Branch {
                from: Some("100-0".into()),
                keep: None
            }
        );
        // A flag after `--branch` is not its id.
        assert_eq!(
            start(&["--branch", "--keep", "3"]),
            Start::Branch {
                from: None,
                keep: None
            }
        );
    }

    #[test]
    fn keep_is_read_as_a_number_and_only_with_branch() {
        let parsed = parse(&args(&["--branch", "100-0", "--keep", "3"])).expect("能解析");
        assert_eq!(
            parsed.start,
            Start::Branch {
                from: Some("100-0".into()),
                keep: None
            }
        );
        assert_eq!(parsed.keep, Some(3));

        assert!(parse(&args(&["--keep", "x"])).is_err(), "不是数字");
        assert!(parse(&args(&["--keep"])).is_err(), "没有值");
        assert!(
            parse(&args(&["--continue", "--keep", "3"])).is_err(),
            "没有 --branch"
        );
    }

    #[test]
    fn an_unknown_flag_is_an_error_not_a_session_named_after_it() {
        // The failure this prevents: `--resum 100-0` silently starting a brand new conversation,
        // which looks exactly like a resume that lost the history.
        let error = parse(&args(&["--resum", "100-0"])).expect_err("该报错");
        assert!(error.contains("--resum"), "{error}");
    }

    #[test]
    fn help_wins_over_anything_else_on_the_line() {
        assert_eq!(start(&["--resume", "100-0", "--help"]), Start::Help);
    }
}
