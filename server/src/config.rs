//! Settings for the Todo Tree language server.
//!
//! These mirror the most useful settings of the original VS Code extension
//! (`todo-tree.general.*`, `todo-tree.regex.*`, `todo-tree.filtering.*`,
//! `todo-tree.highlights.*`), flattened into a single camelCase object that is
//! passed by Zed as `initialization_options` and/or `settings` of the
//! `todo-tree` language server.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

/// Placeholder in [`Config::regex`] that is replaced by an alternation of all tags.
pub const TAGS_PLACEHOLDER: &str = "($TAGS)";

pub const DEFAULT_REGEX: &str = r"(//|#|<!--|;|/\*|^|^[ \t]*(-|\d+.))\s*($TAGS)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    #[serde(alias = "info")]
    Information,
    Hint,
    /// Do not report this tag at all.
    #[serde(alias = "off")]
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HighlightType {
    /// Only the tag itself.
    Tag,
    /// The tag and the text after it.
    Text,
    /// The tag and everything up to the end of the line.
    #[serde(alias = "whole-line")]
    Line,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScanMode {
    /// Scan every file in the workspace, plus any open files.
    #[serde(alias = "workspace and open files")]
    Workspace,
    /// Only scan files that are open in the editor.
    #[serde(alias = "open files")]
    OpenFiles,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Tags to search for (`todo-tree.general.tags`).
    pub tags: Vec<String>,
    /// Groups of tags that are reported under a single name (`todo-tree.general.tagGroups`).
    pub tag_groups: HashMap<String, Vec<String>>,
    /// Regex used to find tags (`todo-tree.regex.regex`). `($TAGS)` is replaced by the tag list.
    pub regex: String,
    /// `todo-tree.regex.regexCaseSensitive`.
    pub regex_case_sensitive: bool,
    /// Regex used to extract a sub tag from the text after the tag (`todo-tree.regex.subTagRegex`).
    pub sub_tag_regex: String,
    /// Allow the regex to match across lines (`todo-tree.regex.enableMultiLine`).
    pub enable_multi_line: bool,
    /// Only files matching one of these globs are scanned (`todo-tree.filtering.includeGlobs`).
    pub include_globs: Vec<String>,
    /// Files matching any of these globs are never scanned (`todo-tree.filtering.excludeGlobs`).
    pub exclude_globs: Vec<String>,
    /// Scan hidden files and folders (`todo-tree.filtering.includeHiddenFiles`).
    pub include_hidden_files: bool,
    /// Respect `.gitignore`, `.ignore` and git exclude files.
    pub use_gitignore: bool,
    /// Skip files bigger than this (in KiB). 0 disables the limit.
    pub max_file_size_kb: u64,
    /// `workspace` or `open-files` (`todo-tree.tree.scanMode`).
    pub scan_mode: ScanMode,
    /// Severity used for tags without an entry in `tagSeverity`.
    pub default_severity: Severity,
    /// Per tag (or tag group) diagnostic severity.
    pub tag_severity: HashMap<String, Severity>,
    /// Which part of the line the diagnostic covers (`todo-tree.highlights.defaultHighlight.type`).
    pub highlight: HighlightType,
    /// Publish TODOs as diagnostics (shown inline and in the project diagnostics panel).
    pub diagnostics: bool,
    /// Maximum number of TODOs reported for the whole workspace. 0 means no limit.
    pub max_results: usize,
}

impl Default for Config {
    fn default() -> Self {
        let mut tag_severity = HashMap::new();
        // Completed markdown tasks are reported, but only as hints.
        tag_severity.insert("[x]".to_string(), Severity::Hint);
        Config {
            tags: ["BUG", "HACK", "FIXME", "TODO", "XXX", "[ ]", "[x]"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            tag_groups: HashMap::new(),
            regex: DEFAULT_REGEX.to_string(),
            regex_case_sensitive: true,
            sub_tag_regex: String::new(),
            enable_multi_line: false,
            include_globs: Vec::new(),
            exclude_globs: vec!["**/node_modules/*/**".to_string()],
            include_hidden_files: false,
            use_gitignore: true,
            max_file_size_kb: 1024,
            scan_mode: ScanMode::Workspace,
            default_severity: Severity::Warning,
            tag_severity,
            highlight: HighlightType::Tag,
            diagnostics: true,
            max_results: 0,
        }
    }
}

impl Config {
    /// Builds a config from a JSON value sent by the client.
    ///
    /// Accepts the settings either directly or nested under a `todo-tree` /
    /// `todoTree` key. Unknown keys are ignored and invalid values fall back
    /// to the defaults (with a message returned so it can be logged).
    pub fn from_value(value: &Value) -> (Config, Option<String>) {
        let value = value
            .get("todo-tree")
            .or_else(|| value.get("todoTree"))
            .unwrap_or(value);
        if value.is_null() {
            return (Config::default(), None);
        }
        match serde_json::from_value::<Config>(value.clone()) {
            Ok(config) => (config, None),
            Err(err) => (
                Config::default(),
                Some(format!("Invalid todo-tree settings, using defaults: {err}")),
            ),
        }
    }

    /// The group a tag belongs to, or the tag itself.
    pub fn group_for_tag<'a>(&'a self, tag: &'a str) -> &'a str {
        for (group, tags) in &self.tag_groups {
            let found = tags.iter().any(|t| {
                if self.regex_case_sensitive {
                    t == tag
                } else {
                    t.eq_ignore_ascii_case(tag)
                }
            });
            if found {
                return group;
            }
        }
        tag
    }

    /// All tags, including those only listed in tag groups.
    pub fn all_tags(&self) -> Vec<String> {
        let mut tags = self.tags.clone();
        for group in self.tag_groups.values() {
            for tag in group {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
        }
        tags
    }

    pub fn severity_for(&self, tag: &str) -> Severity {
        let group = self.group_for_tag(tag);
        let lookup = |key: &str| {
            self.tag_severity.get(key).copied().or_else(|| {
                if self.regex_case_sensitive {
                    None
                } else {
                    self.tag_severity
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(key))
                        .map(|(_, v)| *v)
                }
            })
        };
        lookup(tag)
            .or_else(|| lookup(group))
            .unwrap_or(self.default_severity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_when_null() {
        let (c, err) = Config::from_value(&Value::Null);
        assert!(err.is_none());
        assert_eq!(c.tags.len(), 7);
    }

    #[test]
    fn nested_and_partial() {
        let (c, err) = Config::from_value(&json!({
            "todo-tree": { "tags": ["NOTE"], "tagSeverity": { "NOTE": "error" }, "highlight": "line" }
        }));
        assert!(err.is_none());
        assert_eq!(c.tags, vec!["NOTE"]);
        assert_eq!(c.severity_for("NOTE"), Severity::Error);
        assert_eq!(c.highlight, HighlightType::Line);
        assert_eq!(c.regex, DEFAULT_REGEX);
    }

    #[test]
    fn groups() {
        let (c, _) = Config::from_value(&json!({
            "tagGroups": { "FIXME": ["FIXME", "FIXIT"] },
            "tagSeverity": { "FIXME": "hint" }
        }));
        assert_eq!(c.group_for_tag("FIXIT"), "FIXME");
        assert_eq!(c.severity_for("FIXIT"), Severity::Hint);
        assert!(c.all_tags().contains(&"FIXIT".to_string()));
    }

    #[test]
    fn invalid_falls_back() {
        let (c, err) = Config::from_value(&json!({ "tags": 3 }));
        assert!(err.is_some());
        assert_eq!(c.tags.len(), 7);
    }
}
