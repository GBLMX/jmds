// Ported from GBLMX/pigma (Apache-2.0) and adapted for this workspace — see CREDITS.md and
// THIRD_PARTY_LICENSES/ for what changed and the licence text.
//! Upgrading a `config.toml` that an older build wrote.
//!
//! The file is the user's, so a migration edits **their document** rather than writing a new one:
//! their keys keep their values, their order, their spacing and their comments, and only what the
//! schema requires moves. Two questions drive it — "is this key still part of the schema?" and
//! "what version does this file say it is?" — and both are answered from the document, not from a
//! [`Config`] parsed out of it, because it is the document that is being upgraded.
//!
//! Ported from the previous project (`boxpigma`), where the same machinery was written and tested;
//! what changed here is only the schema it serves. It lives next to the config rather than inside
//! it so the schema file stays readable.

use std::path::Path;

use super::{CONFIG_VERSION, Config};

/// Where a key sits in the user's file: the names — and the array elements — to walk down from the
/// root, as [`stale_keys`] walked them.
#[derive(Debug, Clone)]
enum Step {
    Key(String),
    Element(usize),
}

/// A place a key can be found in — a table (`[a]`, `a.b`, an inline table, the root of the file) or
/// an element of an array — plus the item that *is* a key's value.
enum Node<'a> {
    Item(&'a mut toml_edit::Item),
    Table(&'a mut toml_edit::Table),
    Value(&'a mut toml_edit::Value),
}

impl<'a> Node<'a> {
    /// The entries this node holds, when it holds keys at all.
    fn entries(self) -> Option<&'a mut dyn toml_edit::TableLike> {
        match self {
            Node::Item(item) => item.as_table_like_mut(),
            Node::Table(table) => Some(table),
            Node::Value(toml_edit::Value::InlineTable(table)) => Some(table),
            Node::Value(_) => None,
        }
    }

    /// The element `index` of this node, when it is an array — an array of tables, or one of the
    /// arrays a table writes tables in.
    fn element(self, index: usize) -> Option<Node<'a>> {
        match self {
            Node::Item(toml_edit::Item::ArrayOfTables(arrays)) => {
                Some(Node::Table(arrays.get_mut(index)?))
            }
            Node::Item(toml_edit::Item::Value(toml_edit::Value::Array(array)))
            | Node::Value(toml_edit::Value::Array(array)) => {
                Some(Node::Value(array.get_mut(index)?))
            }
            _ => None,
        }
    }
}

/// Walk `path` down from `node`. `None` cannot happen for a path this module built — the sweep only
/// walks keys it has just looked at — so both callers read it as "nothing there to touch".
fn node_at<'a>(mut node: Node<'a>, path: &[Step]) -> Option<Node<'a>> {
    for step in path {
        node = match step {
            Step::Key(name) => Node::Item(node.entries()?.get_mut(name)?),
            Step::Element(index) => node.element(*index)?,
        };
    }
    Some(node)
}

/// The shapes a key is asked about, in the order they are tried: a number, a string, an array and a
/// table.
const PROBE_SHAPES: [&str; 4] = ["0", "\"\"", "[]", "{}"];

/// Whether [`Config`] still has the key at `path`.
///
/// `Config`'s own `Deserialize` is the one list of keys that exists, so the question goes to it
/// rather than to a list written here: the key is handed a value of every shape a TOML value has
/// (see [`PROBE_SHAPES`]) and the file is read back. A key serde does not know, it *ignores*, and
/// it ignores all four — that is what "the new schema dropped this key" means. A key the schema
/// still has rejects at least one: the four shapes are a number, a string, an array and a table,
/// and every field of the config graph is one of those things or has one inside it (an `Option`, a
/// `Vec`, a `HashMap` — each refuses at least one shape).
///
/// The precondition is that nothing in the graph is a catch-all that swallows all four shapes,
/// which is what makes "all four were ignored" an answer about the schema rather than about the
/// value. `the_sweep_keeps_every_key_the_config_has` pins that precondition: it runs the sweep
/// over a document holding every kind of key this config has and expects nothing to be called
/// stale.
///
/// Every way this can go wrong is read as "keep the key": an unparsable probe, a path that does
/// not resolve, a key the schema does take. Deleting a key the user still needs is the one failure
/// with no recovery from a log line.
fn schema_ignores(doc: &toml_edit::DocumentMut, path: &[Step]) -> bool {
    PROBE_SHAPES
        .iter()
        .all(|shape| schema_takes(doc, path, shape))
}

/// Read the file back with the key at `path` holding what `literal` spells: whether the schema took
/// it. The file is asked on a copy — the user's own document is never touched by a question.
fn schema_takes(doc: &toml_edit::DocumentMut, path: &[Step], literal: &str) -> bool {
    let Ok(value) = literal.parse::<toml_edit::Value>() else {
        // One of ours that does not parse would make every key look unknown; "this shape was not
        // taken" is the reading that keeps the key.
        return false;
    };
    let mut probe = doc.clone();
    let Some(Node::Item(entry)) = node_at(Node::Item(probe.as_item_mut()), path) else {
        return false;
    };
    *entry = toml_edit::Item::Value(value);
    toml_edit::de::from_str::<Config>(&probe.to_string()).is_ok()
}

/// Every key of the user's own file that [`Config`] no longer has, at every level of it.
fn stale_keys(doc: &toml_edit::DocumentMut) -> Vec<Vec<Step>> {
    let mut stale = Vec::new();
    let mut path = Vec::new();
    visit_keys(doc, doc.as_table(), &mut path, &mut stale);
    stale
}

/// One level of the sweep: every key a table holds, and — for a key the schema still has — what is
/// under it. A key that is gone is not looked into: dropping it takes the subtree with it.
fn visit_keys(
    doc: &toml_edit::DocumentMut,
    at: &dyn toml_edit::TableLike,
    path: &mut Vec<Step>,
    stale: &mut Vec<Vec<Step>>,
) {
    for (name, item) in at.iter() {
        path.push(Step::Key(name.to_string()));
        if schema_ignores(doc, path) {
            stale.push(path.clone());
        } else {
            visit_item(doc, item, path, stale);
        }
        path.pop();
    }
}

/// What is under a key the schema has: a table, the elements of an array of tables, or an array —
/// which is another place a table can hide.
fn visit_item(
    doc: &toml_edit::DocumentMut,
    at: &toml_edit::Item,
    path: &mut Vec<Step>,
    stale: &mut Vec<Vec<Step>>,
) {
    match at {
        toml_edit::Item::Table(table) => visit_keys(doc, table, path, stale),
        toml_edit::Item::ArrayOfTables(arrays) => {
            for (index, table) in arrays.iter().enumerate() {
                path.push(Step::Element(index));
                visit_keys(doc, table, path, stale);
                path.pop();
            }
        }
        toml_edit::Item::Value(value) => visit_value(doc, value, path, stale),
        toml_edit::Item::None => {}
    }
}

/// What is under a key the schema has and that is written as a value: an inline table holds keys
/// like any other table, and an array holds them in its elements.
fn visit_value(
    doc: &toml_edit::DocumentMut,
    at: &toml_edit::Value,
    path: &mut Vec<Step>,
    stale: &mut Vec<Vec<Step>>,
) {
    match at {
        toml_edit::Value::InlineTable(table) => visit_keys(doc, table, path, stale),
        toml_edit::Value::Array(array) => {
            for (index, value) in array.iter().enumerate() {
                path.push(Step::Element(index));
                visit_value(doc, value, path, stale);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Take the key at `path` out of the user's document — its own comment goes with it.
fn remove_key(doc: &mut toml_edit::DocumentMut, path: &[Step]) {
    let Some((Step::Key(name), above)) = path.split_last() else {
        return;
    };
    let Some(entries) =
        node_at(Node::Item(doc.as_item_mut()), above).and_then(|node| node.entries())
    else {
        return;
    };
    entries.remove(name);
}

/// The key a path names, the way the user reads it in their file: `api.model`,
/// `panes[0].kind`.
fn path_name(path: &[Step]) -> String {
    let mut name = String::new();
    for step in path {
        match step {
            Step::Key(key) => {
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(key);
            }
            Step::Element(index) => name.push_str(&format!("[{index}]")),
        }
    }
    name
}

/// The schema version the user's file declares.
///
/// Read off the document rather than off a parsed [`Config`], because it is the file that is being
/// upgraded. `config_version` first appeared in v1, so a file that says nothing — or says something
/// that is not a version — is v0: "older than anything this build knows", which is what makes the
/// first migration trigger.
fn document_version(doc: &toml_edit::DocumentMut) -> u32 {
    doc.get("config_version")
        .and_then(toml_edit::Item::as_integer)
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or(0)
}

/// Write the version this build writes, in place: whatever the user has on that line — their own
/// spacing, a comment at the end of it — is the value's decor, and the new value is handed it back.
///
/// A file older than versioning has no such line. The new one is inserted as a root key, which
/// `toml_edit` orders before the tables it knows about; inserting it any other way can land it
/// inside the last `[table]` in the file, where nothing reads it.
fn set_version(doc: &mut toml_edit::DocumentMut) {
    let mut version = toml_edit::Value::from(i64::from(CONFIG_VERSION));
    match doc
        .get_mut("config_version")
        .and_then(toml_edit::Item::as_value_mut)
    {
        Some(existing) => {
            *version.decor_mut() = existing.decor().clone();
            *existing = version;
        }
        None => {
            doc.insert("config_version", toml_edit::Item::Value(version));
        }
    }
}

/// Upgrade the user's own file to [`CONFIG_VERSION`] by editing their document instead of writing a
/// new one. Returns the text to write, or `None` when the file needs nothing — it is already at
/// this schema, or it comes from a newer build and is left as it is (its unknown fields are
/// ignored rather than downgraded).
///
/// The previous file is copied to `config.toml.bak-v{old}` first, so a user can always roll back.
/// The caller writes the result out at once: a migration the user cannot see on disk has not
/// happened as far as the next reader of that file is concerned.
pub(super) fn migrate_document(text: &str, config_path: &Path) -> Option<String> {
    let mut doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    let from = document_version(&doc);
    if from == CONFIG_VERSION {
        return None;
    }
    if from > CONFIG_VERSION {
        log::warn!(
            "{} 是 schema v{from}，这个构建只懂 v{CONFIG_VERSION}：更新的字段会被忽略",
            config_path.display()
        );
        return None;
    }

    let backup = config_path.with_extension(format!("toml.bak-v{from}"));
    match std::fs::copy(config_path, &backup) {
        Ok(_) => log::info!(
            "config.toml schema v{from} → v{CONFIG_VERSION}（已备份到 {}）",
            backup.display()
        ),
        Err(error) => log::warn!(
            "config.toml schema v{from} → v{CONFIG_VERSION}（备份到 {} 失败: {error}）",
            backup.display()
        ),
    }

    // Per-version steps go here, in order, each fixing what the schema it came from got wrong —
    // for example, rewriting a key that was renamed (`if from < 2 { … }`). A step that only
    // *removes* a key needs no code: the sweep below is what takes out anything the schema no
    // longer has.

    // Everything the steps left in the file that the schema no longer has: keys nothing would ever
    // take out again.
    let stale = stale_keys(&doc);
    for path in &stale {
        remove_key(&mut doc, path);
    }
    if !stale.is_empty() {
        let names: Vec<String> = stale.iter().map(|path| path_name(path)).collect();
        log::info!(
            "config.toml schema v{from} → v{CONFIG_VERSION}: 删除新 schema 没有的键 {}",
            names.join("、")
        );
    }

    set_version(&mut doc);
    Some(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sweep(text: &str) -> Vec<String> {
        let doc = text.parse::<toml_edit::DocumentMut>().expect("parses");
        stale_keys(&doc)
            .iter()
            .map(|path| path_name(path))
            .collect()
    }

    #[test]
    fn the_sweep_keeps_every_key_the_config_has() {
        // A document holding one of every kind of value the config graph has. If the sweep calls
        // any of them stale, the probe is answering about the *value* instead of about the schema
        // — and the next migration would delete a key the user still needs.
        let text = r#"
            config_version = 1
            [logger]
            log_level = "debug"
            [api]
            base_url = "https://api.deepseek.com"
            model = "deepseek-chat"
            show_thinking = true
            api_key_env = "DEEPSEEK_API_KEY"
            [editor]
            tab_width = 2
        "#;
        assert_eq!(sweep(text), Vec::<String>::new());
    }

    #[test]
    fn the_sweep_finds_a_key_that_is_not_in_the_schema() {
        // The other half of the same guarantee: the assertion above must not pass just because the
        // sweep never finds anything.
        let text = "config_version = 1\nnonsense = 3\n[api]\nold_model = \"x\"\n";
        assert_eq!(sweep(text), vec!["nonsense", "api.old_model"]);
    }

    #[test]
    fn the_sweep_looks_inside_nested_tables_and_arrays() {
        let text = r#"
            config_version = 1
            [api]
            model = "deepseek-chat"
            [api.legacy]
            nested = 1
            [[panes]]
            kind = "chat"
        "#;
        let found = sweep(text);
        // A nested table the schema does not have is walked into, so the *inner* key is what gets
        // reported: `api.legacy` is a key of `api`, and `api` is a key the schema has.
        assert!(found.contains(&"api.legacy".to_string()), "{found:?}");
        // A key the schema never had is not looked into at all: dropping it takes the subtree with
        // it, so the whole array of tables is one entry, not one per element. (When this config
        // grows a `[[…]]` field of its own, this is the test to extend with the inside-the-array
        // case — the walking code for it is already here.)
        assert!(found.contains(&"panes".to_string()), "{found:?}");
        assert!(
            !found.iter().any(|path| path.starts_with("panes[")),
            "a subtree that is gone is reported once: {found:?}"
        );
    }

    #[test]
    fn removing_a_key_takes_it_out_of_the_document_without_touching_the_rest() {
        let text =
            "# 我的注释\nconfig_version = 1\nobsolete = 1\n[api]\nmodel = \"deepseek-chat\"\n";
        let mut doc: toml_edit::DocumentMut = text.parse().unwrap();
        for path in stale_keys(&doc) {
            remove_key(&mut doc, &path);
        }
        let out = doc.to_string();
        assert!(!out.contains("obsolete"), "{out}");
        assert!(out.contains("# 我的注释"), "{out}");
        assert!(out.contains("model = \"deepseek-chat\""), "{out}");
    }

    #[test]
    fn a_file_without_the_version_key_reads_as_older_than_anything() {
        let doc: toml_edit::DocumentMut = "[api]\nmodel = \"deepseek-chat\"\n".parse().unwrap();
        assert_eq!(document_version(&doc), 0);
        let doc: toml_edit::DocumentMut = "config_version = \"one\"\n".parse().unwrap();
        assert_eq!(document_version(&doc), 0, "not a number is not a version");
    }

    #[test]
    fn setting_the_version_keeps_what_was_on_that_line() {
        let mut doc: toml_edit::DocumentMut =
            "config_version = 1 # 手写的行尾注释\n".parse().unwrap();
        set_version(&mut doc);
        let out = doc.to_string();
        assert!(out.contains("config_version = 1"), "{out}");
        assert!(out.contains("# 手写的行尾注释"), "decor 要留住: {out}");
    }

    #[test]
    fn a_version_key_that_was_missing_lands_before_the_tables() {
        // `toml_edit` writes a root key before the tables; appending it any other way would put it
        // inside the last `[table]`, where `document_version` would never see it and every run
        // would migrate again.
        let mut doc: toml_edit::DocumentMut = "[editor]\ntab_width = 4\n".parse().unwrap();
        set_version(&mut doc);
        let out = doc.to_string();
        let version_at = out.find("config_version").expect("written");
        let table_at = out.find("[editor]").expect("kept");
        assert!(version_at < table_at, "{out}");
    }
}
