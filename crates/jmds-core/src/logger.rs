//! The log sink: a `tracing` subscriber writing into a rolling file.
//!
//! Application code uses the `log` facade; `tracing-log` bridges those records into the
//! subscriber, which `tracing_subscriber::fmt()` installs on its own. What a hand-written sink
//! could not do is done by the appender: files roll daily and the oldest are pruned, so a
//! long-running session cannot grow the log without bound.

use std::path::PathBuf;

use log::Level;
use serde::{Deserialize, Serialize};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{filter::LevelFilter, fmt, fmt::time::LocalTime};

use crate::{config::Config, paths::config_dir};

/// Daily files kept before the appender prunes the oldest.
const LOG_FILES_KEPT: usize = 7;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Logger {
    pub log_level: Level,
}

impl Default for Logger {
    fn default() -> Self {
        // `Info` by default. A debug build's log is the one a developer reads, but the level
        // still has to be safe in a release build: `Debug` traces HTTP bodies.
        Self {
            log_level: Level::Info,
        }
    }
}

/// `log`'s levels and `tracing`'s filters are different types; these five are the mapping.
fn filter(level: Level) -> LevelFilter {
    match level {
        Level::Error => LevelFilter::ERROR,
        Level::Warn => LevelFilter::WARN,
        Level::Info => LevelFilter::INFO,
        Level::Debug => LevelFilter::DEBUG,
        Level::Trace => LevelFilter::TRACE,
    }
}

/// Where the log goes: the current directory in a debug build (so a developer sees it appear next
/// to the code), the config directory in a release build.
fn log_dir() -> PathBuf {
    if cfg!(debug_assertions) {
        PathBuf::from(".")
    } else {
        config_dir()
    }
}

/// Install the subscriber. Called once, before anything that logs.
pub fn init_logger(config: &Config) -> color_eyre::Result<()> {
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("debug.log")
        .max_log_files(LOG_FILES_KEPT)
        .build(log_dir())?;

    fmt()
        .with_writer(appender)
        // The file is read with `tail` and editors, never a terminal: no escape codes.
        .with_ansi(false)
        // Module path per line, which is what makes a log readable after the fact.
        .with_target(true)
        // Local time: the log is read side by side with what the user was doing.
        .with_timer(LocalTime::rfc_3339())
        .with_max_level(filter(config.logger.log_level))
        .try_init()
        .map_err(|error| color_eyre::eyre::eyre!("installing the log subscriber: {error}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_level_maps_to_its_own_filter() {
        // A duplicated arm would silently drop a level's records, which is the kind of thing
        // nobody notices until they need the log.
        let filters = [
            filter(Level::Error),
            filter(Level::Warn),
            filter(Level::Info),
            filter(Level::Debug),
            filter(Level::Trace),
        ];
        for (i, a) in filters.iter().enumerate() {
            for b in filters.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn the_default_level_is_info() {
        assert_eq!(Logger::default().log_level, Level::Info);
    }
}
