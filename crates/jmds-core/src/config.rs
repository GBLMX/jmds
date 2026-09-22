//! `config.toml` — the file this build reads and writes.
//!
//! Two rules shape it. Anything a run *must* be told stays out of it: the API key comes from the
//! environment, named by [`ApiConfig::api_key_env`], so the file can live in a dotfiles repository.
//! And the schema stays small on purpose — every field here is one the app reads, so adding one is
//! a decision rather than a convenience.
//!
//! [`migrate`] holds the machinery that brings an older file up to this schema.

mod migrate;

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use crate::{logger::Logger, paths::config_file};

/// The schema version this build writes.
///
/// A file with no `config_version` reads as v0, which is what makes "older than anything this build
/// knows" expressible before the first rename happens. Bump this when a field is renamed, removed
/// or changes meaning, and add the matching step to [`migrate::migrate_document`].
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub config_version: u32,
    pub logger: Logger,
    pub api: ApiConfig,
    pub editor: EditorConfig,
    pub theme: ThemeConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            logger: Logger::default(),
            api: ApiConfig::default(),
            editor: EditorConfig::default(),
            theme: ThemeConfig::default(),
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
    /// The *name* of the environment variable holding the key — never the key itself, because this
    /// file is meant to be readable in a repository.
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
    /// app would do without writing to the process environment, which every test in the binary
    /// shares.
    pub fn api_key_with(&self, lookup: impl FnOnce(&str) -> Option<String>) -> Option<String> {
        lookup(&self.api_key_env).filter(|key| !key.trim().is_empty())
    }

    pub fn api_key(&self) -> Option<String> {
        self.api_key_with(|name| std::env::var(name).ok())
    }
}

/// How the app should look.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// One of the built-in theme names. An unknown name falls back to the terminal's own colours
    /// rather than failing a run over a typo in a colour scheme.
    pub name: String,
    /// `unicode` or `ascii`.
    pub glyphs: String,
    /// `auto` (probe the terminal), or `truecolor` / `ansi256` / `ansi`.
    pub color_mode: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "terminal".into(),
            glyphs: "unicode".into(),
            color_mode: "auto".into(),
        }
    }
}

/// Editor defaults, read by the editor pane when it opens a buffer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    /// Spaces a `Tab` in the editor inserts. The prompt files are read by a model as much as by a
    /// person, and four is what the tool examples use.
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
    /// A file that does not parse is *not* written over: the user's text stays as it is and the run
    /// continues with defaults, the same rule the log follows. Apart from a migration — which is
    /// the user's own file edited in place, not replaced — loading never writes.
    pub fn load() -> Self {
        Self::load_from(&config_file())
    }

    pub fn load_from(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                log::warn!("{} 读不了: {error} —— 用默认值", path.display());
                return Self::default();
            }
        };

        let mut config: Self = match toml_edit::de::from_str(&text) {
            Ok(config) => config,
            Err(error) => {
                log::warn!(
                    "{} 解析失败 ({error}) —— 用默认值，文件保持原样",
                    path.display()
                );
                return Self::default();
            }
        };

        let Some(migrated) = migrate::migrate_document(&text, path) else {
            // Already this schema, or from a newer build (which `migrate_document` has warned
            // about). Either way the file is left alone.
            return config;
        };

        // A migration edited the file; what this process runs on is what the file now says, read
        // back from the text about to be written — one implementation of the rules, and no chance
        // of the struct and the file disagreeing.
        match toml_edit::de::from_str::<Self>(&migrated) {
            Ok(re_read) => config = re_read,
            Err(error) => log::warn!(
                "{} 迁移后的内容读不回来: {error} —— 继续用迁移前的值",
                path.display()
            ),
        }

        // A writer that produced nothing must not be allowed to erase the user's file.
        if migrated.trim().is_empty() {
            log::error!("迁移没有写出内容，拒绝覆盖 {}", path.display());
            return config;
        }
        match fs::write(path, &migrated) {
            Ok(()) => log::info!("{} 已升到 schema v{CONFIG_VERSION}", path.display()),
            Err(error) => log::warn!("写不回 {}: {error} —— 迁移只在内存里生效", path.display()),
        }
        config
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

    /// The file's text, through the same `toml_edit` that reads it, so what is written can be read
    /// back by the same code path.
    pub fn to_toml(&self) -> String {
        toml_edit::ser::to_string_pretty(self).unwrap_or_else(|error| {
            // A config that cannot be rendered is a bug in this file, not something to hand back to
            // the caller as a `Result` it would have to invent a message for.
            log::error!("the configuration does not serialize: {error}");
            String::new()
        })
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

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn defaults_round_trip_through_the_writer_and_the_reader() {
        let config = Config::default();
        let text = config.to_toml();
        assert!(!text.is_empty());
        assert!(text.contains(&format!("config_version = {CONFIG_VERSION}")));
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
        let garbage = "this is not toml = = =\n";
        write(&path, garbage);

        assert_eq!(Config::load_from(&path), Config::default());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            garbage,
            "a file the user wrote must survive a failed read"
        );
    }

    #[test]
    fn what_saves_can_be_loaded_back() {
        let path = scratch("round-trip");
        let mut config = Config::default();
        config.editor.tab_width = 2;
        config.save_to(&path).unwrap();

        assert_eq!(Config::load_from(&path), config);
        assert!(
            !path.with_extension("toml.bak-v1").exists(),
            "a file this build wrote needs no migration and gets no backup"
        );
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

    #[test]
    fn a_migration_keeps_what_the_user_wrote() {
        let path = scratch("keeps-comments");
        let original = "\
# 我的注释：这一行必须活着
config_version = 0

# 这一条注释属于下面那个已废弃的键，会跟着它一起走
model = \"deepseek-chat\"

# 服务地址（这条注释必须留着）
[api]
base_url = \"https://api.deepseek.com\" # 行尾注释也要留着

# 段落注释
[editor]
tab_width = 2
";
        write(&path, original);
        let config = Config::load_from(&path);
        let after = fs::read_to_string(&path).unwrap();

        for line in [
            "# 我的注释：这一行必须活着",
            "# 服务地址（这条注释必须留着）",
            "# 行尾注释也要留着",
            "# 段落注释",
        ] {
            assert!(after.contains(line), "{line} 不该丢:\n{after}");
        }
        assert!(
            after.contains(&format!("config_version = {CONFIG_VERSION}")),
            "{after}"
        );
        assert!(after.contains("tab_width = 2"), "{after}");
        // The other half of the same rule: a key the schema no longer has leaves with its own
        // comment — it is that key's comment, and nothing else referred to it. (A top-level `model`
        // is stale here: the model lives at `api.model` in this schema.)
        assert!(!after.contains("model ="), "{after}");
        assert!(!after.contains("这一条注释属于"), "{after}");
        // What is read is what the file now says.
        assert_eq!(config.editor.tab_width, 2);
        assert_eq!(config.api.base_url, "https://api.deepseek.com");
    }

    #[test]
    fn a_migration_drops_the_keys_the_schema_no_longer_has_at_every_level() {
        let path = scratch("drops-stale");
        let original = "\
config_version = 0
old_top_level = 1
[api]
model = \"deepseek-chat\"
old_model_field = \"x\"
[api.legacy]
nested = true
[[panes]]
kind = \"chat\"
";
        write(&path, original);
        Config::load_from(&path);
        let after = fs::read_to_string(&path).unwrap();

        for gone in [
            "old_top_level",
            "old_model_field",
            "[api.legacy]",
            "[[panes]]",
        ] {
            assert!(!after.contains(gone), "`{gone}` 该消失:\n{after}");
        }
        // Kept keys keep their values — at the top level and inside a table.
        assert!(after.contains("model = \"deepseek-chat\""), "{after}");
        assert!(
            after.contains(&format!("config_version = {CONFIG_VERSION}")),
            "{after}"
        );
    }

    #[test]
    fn an_unversioned_file_is_migrated_once_and_keeps_its_sections() {
        let path = scratch("unversioned");
        let original = "# 手写注释\n[editor]\ntab_width = 2\n";
        write(&path, original);

        let first = Config::load_from(&path);
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(first.editor.tab_width, 2);
        assert_eq!(first.config_version, CONFIG_VERSION);
        assert!(after.contains("# 手写注释"), "{after}");

        // The version key has to land where a reader looks for it — at the root, before the
        // tables — or the next run migrates again, and again.
        let version_at = after.find("config_version").unwrap();
        let table_at = after.find("[editor]").unwrap();
        assert!(version_at < table_at, "{after}");

        // Loading again changes nothing, and leaves no second backup.
        assert_eq!(Config::load_from(&path), first);
        assert_eq!(fs::read_to_string(&path).unwrap(), after);

        let backup = fs::read_to_string(path.with_extension("toml.bak-v0")).unwrap();
        assert_eq!(backup, original, "备份必须是迁移前的原文");
    }

    #[test]
    fn a_newer_schema_is_read_as_written_and_never_rewritten() {
        let path = scratch("newer");
        let original = "config_version = 99\n[api]\nmodel = \"deepseek-reasoner\"\n";
        write(&path, original);

        let config = Config::load_from(&path);
        assert_eq!(config.config_version, 99);
        assert_eq!(config.api.model, "deepseek-reasoner");
        // The fields a newer build added are simply absent from this build's view of it.
        assert_eq!(config.api.base_url, ApiConfig::default().base_url);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "a file from the future must not be downgraded"
        );
        assert!(!path.with_extension("toml.bak-v99").exists());
    }
}
