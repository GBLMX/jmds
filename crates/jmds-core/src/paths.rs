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
}
