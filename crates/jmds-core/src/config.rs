//! `config.toml` — the file this build reads and writes.
//!
//! Two rules shape it. Anything a run *must* be told stays out of it: the API key comes from the
//! environment, named by [`ApiConfig::api_key_env`], so the file can live in a dotfiles
//! repository. And the schema stays small on purpose — every field here is one the app reads, so
//! adding one is a decision rather than a convenience.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use crate::{logger::Logger, paths::config_file};

/// The schema version this build writes.
///
/// A file with no `config_version` reads as v0 (serde fills the number in as zero), which is what
/// makes "older than this build" expressible before the first rename happens. Bump this when a
/// field is renamed, removed or changes meaning.
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub config_version: u32,
    pub logger: Logger,
    pub api: ApiConfig,
    pub editor: EditorConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            logger: Logger::default(),
            api: ApiConfig::default(),
            editor: EditorConfig::default(),
        }
    }
}

/// How to reach DeepSeek.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub base_url: String,
    /// Which model to call. `deepseek-chat` is the default; the reasoning model is the one whose
    /// answers carry `reasoning_content`.
    pub model: String,
    /// Whether the chat pane renders the reasoning. It is a display choice, not a request
    /// parameter: the API sends `reasoning_content` when the model produces it.
    pub show_thinking: bool,
    /// The *name* of the environment variable holding the key — never the key itself, because
    /// this file is meant to be readable in a repository.
    pub api_key_env: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-chat".into(),
            show_thinking: true,
            api_key_env: "DEEPSEEK_API_KEY".into(),
        }
    }
}

impl ApiConfig {
    /// The key, using a supplied lookup.
    ///
    /// The lookup is a parameter rather than a call to `std::env::var` so a test can ask what the
    /// app would do without writing to the process environment, which is shared by every test in
    /// the binary.
    pub fn api_key_with(&self, lookup: impl FnOnce(&str) -> Option<String>) -> Option<String> {
        lookup(&self.api_key_env).filter(|key| !key.trim().is_empty())
    }

    pub fn api_key(&self) -> Option<String> {
        self.api_key_with(|name| std::env::var(name).ok())
    }
}

/// Editor defaults, read by the editor pane when it opens a buffer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    /// Spaces a `Tab` in the editor inserts. The prompt files are read by a model as much as by
    /// a person, and four is what the toolset's own examples use.
    pub tab_width: u8,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self { tab_width: 4 }
    }
}

impl Config {
    /// Read the configuration file, or defaults.
    ///
    /// A file that does not parse is *not* written over: the user's text stays as it is and the
    /// run continues with defaults, the same rule the log directory follows. Loading never
    /// writes — saving is something the user asks for.
    pub fn load() -> Self {
        Self::load_from(&config_file())
    }

    pub fn load_from(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                log::warn!("cannot read {}: {error} — using defaults", path.display());
                return Self::default();
            }
        };

        match toml_edit::de::from_str::<Self>(&text) {
            Ok(config) => {
                config.check_version(path);
                config
            }
            Err(error) => {
                log::warn!(
                    "{} does not parse ({error}) — using defaults, leaving the file alone",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Write this configuration where [`Self::load`] would look for it.
    pub fn save(&self) -> io::Result<()> {
        self.save_to(&config_file())
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, self.to_toml())
    }

    /// The file's text, through the same `toml_edit` that reads it, so what is written can be
    /// read back by the same code path.
    pub fn to_toml(&self) -> String {
        toml_edit::ser::to_string_pretty(self).unwrap_or_else(|error| {
            // A config that cannot be rendered is a bug in this file, not something to hand back
            // to the caller as a `Result` it would have to invent a message for.
            log::error!("the configuration does not serialize: {error}");
            String::new()
        })
    }

    /// Say so when the file is from a newer build.
    ///
    /// It is still read for the fields this build knows — refusing it would break a downgrade
    /// for no gain — but silently ignoring a version is how someone loses a setting they think
    /// they set.
    fn check_version(&self, path: &Path) {
        if self.config_version > CONFIG_VERSION {
            log::warn!(
                "{} is schema v{}; this build understands v{CONFIG_VERSION} — newer fields are ignored",
                path.display(),
                self.config_version
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A file of its own per test: the suite runs in parallel, and these tests write.
    fn scratch(name: &str) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "jmds-config-{}-{}-{}",
            std::process::id(),
            name,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir.join("config.toml")
    }

    #[test]
    fn defaults_round_trip_through_the_writer_and_the_reader() {
        let config = Config::default();
        let text = config.to_toml();
        assert!(!text.is_empty());
        let back: Config = toml_edit::de::from_str(&text).expect("the writer's output parses");
        assert_eq!(back, config);
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let path = scratch("missing");
        assert_eq!(Config::load_from(&path), Config::default());
    }

    #[test]
    fn a_file_that_does_not_parse_yields_defaults_and_is_left_alone() {
        let path = scratch("garbage");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let garbage = "this is not toml = = =\n";
        fs::write(&path, garbage).unwrap();

        assert_eq!(Config::load_from(&path), Config::default());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            garbage,
            "a file the user wrote must survive a failed read"
        );
    }

    #[test]
    fn a_newer_schema_is_read_as_written() {
        let path = scratch("newer");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "config_version = 99\n[api]\nmodel = \"deepseek-reasoner\"\n",
        )
        .unwrap();

        let config = Config::load_from(&path);
        assert_eq!(config.config_version, 99);
        assert_eq!(config.api.model, "deepseek-reasoner");
        // The fields a newer build added are simply absent from this build's view of it.
        assert_eq!(config.api.base_url, ApiConfig::default().base_url);
    }

    #[test]
    fn what_saves_can_be_loaded_back() {
        let path = scratch("round-trip");
        let mut config = Config::default();
        config.editor.tab_width = 2;
        config.save_to(&path).unwrap();

        assert_eq!(Config::load_from(&path), config);
    }

    #[test]
    fn the_api_key_is_read_from_the_named_variable_and_never_written() {
        let api = ApiConfig {
            api_key_env: "JMDS_TEST_KEY".into(),
            ..ApiConfig::default()
        };
        assert_eq!(
            api.api_key_with(|name| (name == "JMDS_TEST_KEY").then(|| "sk-secret".to_string())),
            Some("sk-secret".to_string())
        );
        // Whitespace-only is what an unset variable in a shell script looks like.
        assert_eq!(api.api_key_with(|_| Some("  ".into())), None);
        assert_eq!(api.api_key_with(|_| None), None);

        let written = Config {
            api,
            ..Config::default()
        }
        .to_toml();
        assert!(written.contains("JMDS_TEST_KEY"), "{written}");
        assert!(
            !written.contains("sk-"),
            "the file carries the variable's name, not a key: {written}"
        );
    }
}
