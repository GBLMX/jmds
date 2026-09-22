//! Where the app keeps its files.
//!
//! Everything is under one directory per kind, named after the app, resolved through `dirs` so
//! the XDG variables (and the platform equivalents on macOS and Windows) are honoured. The
//! fallback is the working directory rather than `~`: the point of the fallback is to keep a
//! machine with no home directory writable, and `./.` always is.

use std::path::PathBuf;

const APP: &str = "jmds";

/// `~/.config/jmds` — the configuration file and the log directory in a release build.
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP)
}

/// `~/.cache/jmds` — sessions, caches, anything reproducible.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP)
}

/// The configuration file this build reads and writes.
pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

/// `~/x` — what a person, or a model, writes.
///
/// The shell is not on the path when the path comes from a tool call or a prompt file, so a `~`
/// would reach the file system as a literal directory name. One function for every path that
/// arrives from outside the program.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(rest),
        // `~user` is not expanded: this is one user's tool, and guessing another account's home
        // from a name is worse than letting the path fail.
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_path_is_named_after_the_app() {
        // The failure this guards against is a rename that misses one of them: two directories
        // under two names is how a config file "disappears" after an upgrade.
        for path in [config_dir(), cache_dir(), config_file()] {
            assert!(
                path.components().any(|c| c.as_os_str() == APP),
                "{} is not under {APP}",
                path.display()
            );
        }
        assert_eq!(config_file(), config_dir().join("config.toml"));
    }

    #[test]
    fn a_tilde_becomes_the_home_directory_and_other_paths_are_left_alone() {
        assert_eq!(expand_tilde("~"), dirs::home_dir().unwrap());
        assert_eq!(expand_tilde("~/x"), dirs::home_dir().unwrap().join("x"));
        assert_eq!(expand_tilde("/abs/x"), PathBuf::from("/abs/x"));
        assert_eq!(expand_tilde("rel/x"), PathBuf::from("rel/x"));
        assert_eq!(
            expand_tilde("~other/x"),
            PathBuf::from("~other/x"),
            "another user's home is not guessed"
        );
    }
}
