//! Walking a workspace folder and scanning its files (replaces the ripgrep
//! wrapper of the original extension).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{WalkBuilder, WalkState};

use crate::config::Config;
use crate::scanner::{Scanner, Todo};

pub type Results = BTreeMap<PathBuf, Vec<Todo>>;

/// Result of scanning a workspace folder.
#[derive(Default)]
pub struct ScanOutput {
    /// Files that contain at least one TODO.
    pub results: Results,
    /// Every file that passed the filters and was scanned.
    pub files: HashSet<PathBuf>,
}

/// Include / exclude filtering shared by workspace scans and single files.
pub struct Filter {
    root: PathBuf,
    include: Option<GlobSet>,
    exclude: GlobSet,
    include_hidden: bool,
    max_size: Option<u64>,
}

fn build_globs(globs: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        let glob = glob.trim();
        if glob.is_empty() {
            continue;
        }
        let parsed = globset::GlobBuilder::new(glob)
            .literal_separator(true)
            .backslash_escape(false)
            .build()
            .or_else(|_| Glob::new(glob))
            .map_err(|e| format!("Invalid glob '{glob}': {e}"))?;
        builder.add(parsed);
    }
    builder.build().map_err(|e| e.to_string())
}

impl Filter {
    pub fn new(root: &Path, config: &Config) -> Result<Filter, String> {
        let include = if config.include_globs.iter().all(|g| g.trim().is_empty()) {
            None
        } else {
            Some(build_globs(&config.include_globs)?)
        };
        Ok(Filter {
            root: root.to_path_buf(),
            include,
            exclude: build_globs(&config.exclude_globs)?,
            include_hidden: config.include_hidden_files,
            max_size: (config.max_file_size_kb > 0).then(|| config.max_file_size_kb * 1024),
        })
    }

    fn matches(set: &GlobSet, root: &Path, path: &Path) -> bool {
        if set.is_match(path) {
            return true;
        }
        match path.strip_prefix(root) {
            Ok(rel) => set.is_match(rel),
            Err(_) => false,
        }
    }

    pub fn is_excluded_dir(&self, path: &Path) -> bool {
        Self::matches(&self.exclude, &self.root, path)
    }

    /// Whether a file passes the glob filters (ignore files are handled by the walker).
    pub fn accepts_file(&self, path: &Path) -> bool {
        if Self::matches(&self.exclude, &self.root, path) {
            return false;
        }
        if let Some(include) = &self.include {
            if !Self::matches(include, &self.root, path) {
                return false;
            }
        }
        if !self.include_hidden {
            if let Ok(rel) = path.strip_prefix(&self.root) {
                let hidden = rel.components().any(|c| {
                    c.as_os_str()
                        .to_str()
                        .map(|s| s.starts_with('.') && s != "." && s != "..")
                        .unwrap_or(false)
                });
                if hidden {
                    return false;
                }
            }
        }
        true
    }

    fn too_big(&self, len: u64) -> bool {
        self.max_size.map(|max| len > max).unwrap_or(false)
    }
}

/// Reads a file as text, skipping binary files.
pub fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return None;
    }
    Some(match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

/// Scans a single file on disk.
pub fn scan_file(path: &Path, filter: &Filter, scanner: &Scanner) -> Vec<Todo> {
    if let Ok(meta) = std::fs::metadata(path) {
        if !meta.is_file() || filter.too_big(meta.len()) {
            return Vec::new();
        }
    }
    match read_text(path) {
        Some(text) if scanner.might_match(&text) => scanner.scan(&text),
        _ => Vec::new(),
    }
}

/// Scans every file under `root`, in parallel, honouring the configured filters.
pub fn scan_root(root: &Path, config: &Config, scanner: &Scanner, filter: &Filter) -> ScanOutput {
    let output = Mutex::new(ScanOutput::default());
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!config.include_hidden_files)
        .git_ignore(config.use_gitignore)
        .git_global(config.use_gitignore)
        .git_exclude(config.use_gitignore)
        .ignore(config.use_gitignore)
        .parents(config.use_gitignore)
        .require_git(false)
        .follow_links(false);
    if let Some(max) = filter.max_size {
        builder.max_filesize(Some(max));
    }
    // Never descend into the git directory itself.
    builder.filter_entry(|entry| entry.file_name() != ".git");

    builder.build_parallel().run(|| {
        let output = &output;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => return WalkState::Continue,
            };
            let path = entry.path();
            match entry.file_type() {
                Some(ft) if ft.is_dir() => {
                    if entry.depth() > 0 && filter.is_excluded_dir(path) {
                        return WalkState::Skip;
                    }
                    return WalkState::Continue;
                }
                Some(ft) if ft.is_file() => {}
                _ => return WalkState::Continue,
            }
            if !filter.accepts_file(path) {
                return WalkState::Continue;
            }
            let todos = scan_file(path, filter, scanner);
            let mut output = output.lock().unwrap();
            output.files.insert(path.to_path_buf());
            if !todos.is_empty() {
                output.results.insert(path.to_path_buf(), todos);
            }
            WalkState::Continue
        })
    });
    output.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn walks_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::write(root.join("src/a.rs"), "// TODO a\n// FIXME b\n").unwrap();
        fs::write(root.join("src/b.py"), "# nothing here\n").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "// TODO dep\n").unwrap();
        fs::write(root.join("ignored/x.rs"), "// TODO ignored\n").unwrap();
        fs::write(root.join(".hidden/x.rs"), "// TODO hidden\n").unwrap();
        fs::write(root.join("bin.dat"), b"// TODO \0binary").unwrap();

        let (config, _) = Config::from_value(&json!({}));
        let scanner = Scanner::new(&config).unwrap();
        let filter = Filter::new(root, &config).unwrap();
        let results = scan_root(root, &config, &scanner, &filter).results;
        let files: Vec<_> = results
            .keys()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(files, vec!["src/a.rs"]);
        assert_eq!(results.values().next().unwrap().len(), 2);

        let (config, _) = Config::from_value(&json!({
            "includeGlobs": ["**/*.js"], "excludeGlobs": [], "useGitignore": false, "includeHiddenFiles": true
        }));
        let filter = Filter::new(root, &config).unwrap();
        let results = scan_root(root, &config, &scanner, &filter).results;
        let files: Vec<_> = results
            .keys()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(files, vec!["node_modules/pkg/index.js"]);
    }
}
