//! Finding tags in text. This is the Rust equivalent of the regex building and
//! tag extraction in the original extension's `utils.js`.

use regex::{Regex, RegexBuilder};

use crate::config::{Config, HighlightType, TAGS_PLACEHOLDER};

/// A position in a document: zero based line and UTF-16 column (the LSP default encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: u32,
    pub character: u32,
}

/// One TODO found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    /// The tag as configured (e.g. `TODO`), even if matched case-insensitively.
    pub tag: String,
    /// Tag group name, or the tag itself.
    pub group: String,
    /// Optional sub tag extracted with `subTagRegex`.
    pub sub_tag: Option<String>,
    /// The text after the tag, cleaned up.
    pub text: String,
    /// Start of the highlighted range (start of the tag).
    pub start: Pos,
    /// End of the highlighted range (depends on the `highlight` setting).
    pub end: Pos,
}

impl Todo {
    /// Label used for diagnostics and symbols, e.g. `TODO(ui): fix the layout`.
    pub fn label(&self) -> String {
        let mut label = self.tag.clone();
        if let Some(sub) = &self.sub_tag {
            label.push('(');
            label.push_str(sub);
            label.push(')');
        }
        if !self.text.is_empty() {
            label.push_str(": ");
            label.push_str(&self.text);
        }
        label
    }
}

pub struct Scanner {
    regex: Regex,
    tag_regex: Option<Regex>,
    sub_tag_regex: Option<Regex>,
    tags: Vec<String>,
    case_sensitive: bool,
    multi_line: bool,
    highlight: HighlightType,
    groups: Vec<(String, String)>,
}

fn escape_tag(tag: &str) -> String {
    regex::escape(tag)
}

/// Builds `(TAG1|TAG2|...)`, longest-first so that e.g. `TODOS` wins over `TODO`.
pub fn tag_alternation(tags: &[String]) -> String {
    let mut tags: Vec<&String> = tags.iter().filter(|t| !t.is_empty()).collect();
    tags.sort();
    tags.reverse();
    let parts: Vec<String> = tags.iter().map(|t| escape_tag(t)).collect();
    format!("({})", parts.join("|"))
}

impl Scanner {
    pub fn new(config: &Config) -> Result<Scanner, String> {
        let tags = config.all_tags();
        let alternation = tag_alternation(&tags);
        let uses_tags = config.regex.contains(TAGS_PLACEHOLDER);
        let source = config.regex.replace(TAGS_PLACEHOLDER, &alternation);
        let build = |src: &str| {
            RegexBuilder::new(src)
                .case_insensitive(!config.regex_case_sensitive)
                .multi_line(true)
                .dot_matches_new_line(config.enable_multi_line)
                .build()
                .map_err(|e| format!("Invalid regex '{src}': {e}"))
        };
        let regex = build(&source)?;
        let tag_regex = if uses_tags && !tags.is_empty() {
            Some(build(&alternation)?)
        } else {
            None
        };
        let sub_tag_regex = if config.sub_tag_regex.trim().is_empty() {
            None
        } else {
            Some(build(&config.sub_tag_regex)?)
        };
        let groups = tags
            .iter()
            .map(|t| (t.clone(), config.group_for_tag(t).to_string()))
            .collect();
        Ok(Scanner {
            regex,
            tag_regex,
            sub_tag_regex,
            tags,
            case_sensitive: config.regex_case_sensitive,
            multi_line: config.enable_multi_line,
            highlight: config.highlight,
            groups,
        })
    }

    /// Cheap pre-check used to skip files that cannot contain a match.
    pub fn might_match(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }

    pub fn scan(&self, text: &str) -> Vec<Todo> {
        let index = LineIndex::new(text);
        let mut todos = Vec::new();
        if self.multi_line {
            for m in self.regex.find_iter(text) {
                if m.as_str().is_empty() {
                    continue;
                }
                if let Some(todo) = self.make_todo(text, &index, m.start(), m.end()) {
                    todos.push(todo);
                }
            }
        } else {
            for (line_no, &line_start) in index.starts.iter().enumerate() {
                let line_end = index.line_end(text, line_no);
                let line = &text[line_start..line_end];
                for m in self.regex.find_iter(line) {
                    if m.as_str().is_empty() {
                        continue;
                    }
                    if let Some(todo) =
                        self.make_todo(text, &index, line_start + m.start(), line_start + m.end())
                    {
                        todos.push(todo);
                    }
                }
            }
        }
        todos
    }

    fn canonical_tag(&self, found: &str) -> String {
        self.tags
            .iter()
            .find(|t| {
                if self.case_sensitive {
                    t.as_str() == found
                } else {
                    t.to_lowercase() == found.to_lowercase()
                }
            })
            .cloned()
            .unwrap_or_else(|| found.to_string())
    }

    fn group(&self, tag: &str) -> String {
        self.groups
            .iter()
            .find(|(t, _)| t == tag)
            .map(|(_, g)| g.clone())
            .unwrap_or_else(|| tag.to_string())
    }

    fn make_todo(&self, text: &str, index: &LineIndex, start: usize, end: usize) -> Option<Todo> {
        let matched = &text[start..end];

        // Locate the tag inside the match. With a custom regex that has no
        // `($TAGS)` placeholder, the whole match is treated as the tag.
        let (tag_start, tag_end) = match &self.tag_regex {
            Some(tag_regex) => match tag_regex.find(matched) {
                Some(t) => (start + t.start(), start + t.end()),
                None => (start, end),
            },
            None => {
                let trimmed_start = matched.len() - matched.trim_start().len();
                (start + trimmed_start, start + matched.trim_end().len())
            }
        };
        if tag_start >= tag_end {
            return None;
        }
        let tag = self.canonical_tag(text[tag_start..tag_end].trim());

        // Text after the tag: the rest of the match in multi-line mode, plus
        // the rest of the line the match ends on.
        let end_line = index.line_of(end);
        let end_line_end = index.line_end(text, end_line);
        let after_end = end_line_end.max(tag_end);
        let raw_after = &text[tag_end..after_end];
        let content_end = tag_end + content_len(raw_after);

        let mut after = raw_after.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut sub_tag = None;
        if let Some(sub_tag_regex) = &self.sub_tag_regex {
            let trimmed = after.trim().to_string();
            if let Some(caps) = sub_tag_regex.captures(&trimmed) {
                sub_tag = caps
                    .get(1)
                    .map(|m| m.as_str().trim().to_string())
                    .filter(|s| !s.is_empty());
                let whole = caps.get(0).unwrap();
                after = format!("{}{}", &trimmed[..whole.start()], &trimmed[whole.end()..]);
            }
        }
        let text_after = clean_text(&after);

        let highlight_end = match self.highlight {
            HighlightType::Tag => tag_end,
            HighlightType::Text => content_end,
            HighlightType::Line => after_end,
        };

        Some(Todo {
            group: self.group(&tag),
            tag,
            sub_tag,
            text: text_after,
            start: index.pos(text, tag_start),
            end: index.pos(text, highlight_end),
        })
    }
}

/// Length of `s` without trailing whitespace and comment terminators.
fn content_len(s: &str) -> usize {
    let mut t = s.trim_end();
    loop {
        let before = t.len();
        for closer in ["*/", "-->", "#}", "%}", "--}}", "}}"] {
            if let Some(stripped) = t.strip_suffix(closer) {
                t = stripped.trim_end();
            }
        }
        if t.len() == before {
            break;
        }
    }
    t.len()
}

/// Removes separators after the tag (`TODO: x`, `TODO - x`) and comment terminators.
fn clean_text(s: &str) -> String {
    let s = &s[..content_len(s)];
    let s = s.trim_start();
    let s = s
        .strip_prefix(':')
        .or_else(|| s.strip_prefix(" -"))
        .or_else(|| s.strip_prefix('-'))
        .unwrap_or(s);
    s.trim().to_string()
}

/// Maps byte offsets to LSP positions.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        LineIndex { starts }
    }

    fn line_of(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    /// Byte offset of the end of the line, excluding `\r\n` / `\n`.
    fn line_end(&self, text: &str, line: usize) -> usize {
        let end = self
            .starts
            .get(line + 1)
            .map(|s| s - 1)
            .unwrap_or(text.len());
        let start = self.starts[line];
        if end > start && text.as_bytes()[end - 1] == b'\r' {
            end - 1
        } else {
            end
        }
    }

    pub fn pos(&self, text: &str, offset: usize) -> Pos {
        let line = self.line_of(offset);
        let start = self.starts[line];
        let character = text[start..offset].encode_utf16().count() as u32;
        Pos {
            line: line as u32,
            character,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scanner(settings: serde_json::Value) -> Scanner {
        let (config, err) = Config::from_value(&settings);
        assert!(err.is_none(), "{err:?}");
        Scanner::new(&config).unwrap()
    }

    #[test]
    fn finds_default_tags() {
        let s = scanner(json!({}));
        let text = "fn main() {\n    // TODO: write code\n    let x = 1; # FIXME broken */\n}\n";
        let todos = s.scan(text);
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].tag, "TODO");
        assert_eq!(todos[0].text, "write code");
        assert_eq!(
            todos[0].start,
            Pos {
                line: 1,
                character: 7
            }
        );
        assert_eq!(
            todos[0].end,
            Pos {
                line: 1,
                character: 11
            }
        );
        assert_eq!(todos[1].tag, "FIXME");
        assert_eq!(todos[1].text, "broken");
        assert_eq!(todos[1].label(), "FIXME: broken");
    }

    #[test]
    fn ignores_tags_outside_comments() {
        let s = scanner(json!({}));
        assert!(s.scan("let TODO = 1;\n").is_empty());
    }

    #[test]
    fn markdown_checkboxes() {
        let s = scanner(json!({}));
        let todos = s.scan("- [ ] open task\n- [x] done task\n1. [ ] numbered\n");
        let tags: Vec<_> = todos.iter().map(|t| t.tag.as_str()).collect();
        assert_eq!(tags, vec!["[ ]", "[x]", "[ ]"]);
        assert_eq!(todos[0].text, "open task");
    }

    #[test]
    fn case_insensitive() {
        let s = scanner(json!({ "regexCaseSensitive": false }));
        let todos = s.scan("// todo lower case\n");
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].tag, "TODO");
    }

    #[test]
    fn sub_tags() {
        let s = scanner(json!({ "subTagRegex": r"^\(([^)]*)\)\s*:?" }));
        let todos = s.scan("// TODO(alice): refactor\n");
        assert_eq!(todos[0].sub_tag.as_deref(), Some("alice"));
        assert_eq!(todos[0].text, "refactor");
        assert_eq!(todos[0].label(), "TODO(alice): refactor");
    }

    #[test]
    fn highlight_modes() {
        let text = "/* TODO fix me */   \n";
        let t = &scanner(json!({ "highlight": "text" })).scan(text)[0];
        assert_eq!(t.end.character, 14);
        let t = &scanner(json!({ "highlight": "line" })).scan(text)[0];
        assert_eq!(t.end.character, 20);
    }

    #[test]
    fn utf16_columns() {
        let s = scanner(json!({}));
        let todos = s.scan("let s = \"😀\"; // TODO emoji\n");
        // The emoji is two UTF-16 code units.
        assert_eq!(todos[0].start.character, 17);
    }

    #[test]
    fn crlf_lines() {
        let s = scanner(json!({ "highlight": "line" }));
        let todos = s.scan("// TODO one\r\n// TODO two\r\n");
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].text, "one");
        assert_eq!(
            todos[0].end,
            Pos {
                line: 0,
                character: 11
            }
        );
    }

    #[test]
    fn custom_regex_without_placeholder() {
        let s = scanner(json!({ "regex": r"@todo\b" }));
        let todos = s.scan("/** @todo document */\n");
        assert_eq!(todos[0].tag, "@todo");
        assert_eq!(todos[0].text, "document");
    }

    #[test]
    fn multi_line() {
        let s = scanner(json!({
            "enableMultiLine": true,
            "regex": r"(//|#)\s*($TAGS)[^\n]*(\n\s*//\s{2,}[^\n]*)*"
        }));
        let todos = s.scan("// TODO first line\n//   continued\nlet x;\n");
        assert_eq!(todos.len(), 1);
        assert_eq!(
            todos[0].text,
            "first line //   continued"
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    #[test]
    fn tag_groups() {
        let s = scanner(json!({ "tagGroups": { "FIXME": ["FIXME", "FIXIT"] } }));
        let todos = s.scan("// FIXIT now\n");
        assert_eq!(todos[0].tag, "FIXIT");
        assert_eq!(todos[0].group, "FIXME");
    }
}
