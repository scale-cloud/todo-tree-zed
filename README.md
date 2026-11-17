# Todo Tree for Zed

This is a port of the [Todo Tree](https://github.com/Gruntfuggly/todo-tree) VS Code extension to the [Zed](https://zed.dev) editor. It searches your project for comment tags like `TODO`, `FIXME` and `BUG`, plus markdown checkboxes (`- [ ]`), and lists them in Zed.

```rust
// TODO: handle the error case
fn parse() {} // FIXME this breaks on empty input
```

<img src="resources/todo-tree.png" width="64" alt="Todo Tree icon">

## How it works in Zed

Zed extensions can't add custom tree views or text decorations. So this port does the scanning in a small language server, **`todo-tree-ls`** (written in Rust, in [`server/`](server/)), and reports what it finds through the features Zed already has:

| VS Code Todo Tree | Zed |
| --- | --- |
| The tree view in the activity bar | The **project diagnostics** panel (`editor: toggle project diagnostics`, <kbd>cmd/ctrl-shift-m</kbd>). Each TODO is a diagnostic, grouped by file. |
| Searching or filtering the tree | **Project symbol search** (`project symbols: toggle`, <kbd>cmd/ctrl-t</kbd>). Every TODO is a workspace symbol named like `FIXME: this breaks on empty input`, and you can fuzzy-search them. |
| Highlighting tags in the editor | The diagnostic underline and gutter marker on each tag. Set `"diagnostics": { "inline": { "enabled": true } }` in Zed to show the TODO text at the end of the line too. |
| Going to the next or previous TODO | `editor: go to diagnostic` / `editor: go to previous diagnostic` (<kbd>F8</kbd> / <kbd>shift-F8</kbd>) |
| ripgrep search | A built-in parallel file walker that respects `.gitignore`, built on the same `ignore`/`regex` crates as ripgrep. |
| Status bar count | A "Scanning for TODOs… N TODOs found" progress message from the language server |

The server rescans a file whenever you edit, save, open or close it, or change it outside Zed. Unsaved edits show up right away.

## Installation

### From source (dev extension)

1. Install Rust with [rustup](https://rustup.rs). Zed needs a rustup-managed toolchain to build extensions.
2. Build the language server and put it on your `PATH`:

   ```bash
   cargo install --path server
   ```

3. In Zed, run `zed: install dev extension` and select this repository's folder.

If `todo-tree-ls` isn't on your `PATH`, the extension downloads the binary for your platform from the latest [GitHub release](https://github.com/JordanRickman/todo-tree/releases) of this repository. The [release workflow](.github/workflows/release.yml) builds those binaries when a `v*` tag is pushed.

You can also point Zed at a specific binary:

```json
{
  "lsp": {
    "todo-tree": {
      "binary": { "path": "/path/to/todo-tree-ls" }
    }
  }
}
```

## Configuration

All settings go under `lsp.todo-tree.settings` in your Zed `settings.json`, either the user file or a project's `.zed/settings.json`. Every key is optional. These are the defaults:

```json
{
  "lsp": {
    "todo-tree": {
      "settings": {
        "tags": ["BUG", "HACK", "FIXME", "TODO", "XXX", "[ ]", "[x]"],
        "tagGroups": {},
        "regex": "(//|#|<!--|;|/\\*|^|^[ \\t]*(-|\\d+.))\\s*($TAGS)",
        "regexCaseSensitive": true,
        "subTagRegex": "",
        "enableMultiLine": false,
        "includeGlobs": [],
        "excludeGlobs": ["**/node_modules/*/**"],
        "includeHiddenFiles": false,
        "useGitignore": true,
        "maxFileSizeKb": 1024,
        "scanMode": "workspace",
        "defaultSeverity": "warning",
        "tagSeverity": { "[x]": "hint" },
        "highlight": "tag",
        "diagnostics": true,
        "maxResults": 0
      }
    }
  }
}
```

| Setting | VS Code equivalent | Description |
| --- | --- | --- |
| `tags` | `todo-tree.general.tags` | Tags to look for. |
| `tagGroups` | `todo-tree.general.tagGroups` | Report several tags under one name, e.g. `{ "FIXME": ["FIXME", "FIXIT", "FIX"] }`. The group name is used as the diagnostic code and for `tagSeverity`. |
| `regex` | `todo-tree.regex.regex` | Regex used to find tags. `($TAGS)` is replaced by the list of tags. The default only matches tags after a comment marker or at the start of a line (for markdown task lists). It uses [Rust regex syntax](https://docs.rs/regex/latest/regex/#syntax), which is the same as ripgrep's default and has no look-around. |
| `regexCaseSensitive` | `todo-tree.regex.regexCaseSensitive` | Set to `false` to also match `todo`, `Todo` and so on. |
| `subTagRegex` | `todo-tree.regex.subTagRegex` | A regex run on the text after the tag. Capture group 1 becomes a sub tag, and the matched text is removed. For example, with `"^\\(([^)]*)\\)\\s*:?"`, `TODO(alice): fix` gets sub tag `alice` and text `fix`. |
| `enableMultiLine` | `todo-tree.regex.enableMultiLine` | Match the regex against whole files instead of line by line, so a match can span lines. `.` also matches newlines in this mode, so use `[^\n]` for "rest of line". |
| `includeGlobs` | `todo-tree.filtering.includeGlobs` | When not empty, only matching files are scanned. Globs match paths relative to the worktree root (absolute paths work too). |
| `excludeGlobs` | `todo-tree.filtering.excludeGlobs` | Files and folders that are never scanned. |
| `includeHiddenFiles` | `todo-tree.filtering.includeHiddenFiles` | Scan dot-files and dot-folders. |
| `useGitignore` | ripgrep's default behaviour | Respect `.gitignore`, `.ignore` and `.git/info/exclude`. |
| `maxFileSizeKb` | `todo-tree.ripgrep.ripgrepMaxBuffer` (roughly) | Skip files bigger than this. Set to `0` for no limit. Binary files are always skipped. |
| `scanMode` | `todo-tree.tree.scanMode` | `"workspace"` scans the whole worktree plus open files. `"open-files"` only scans files that are open in Zed. |
| `defaultSeverity` | n/a | Diagnostic severity for tags: `"error"`, `"warning"`, `"information"`, `"hint"` or `"none"`. |
| `tagSeverity` | n/a | Severity for each tag or tag group, e.g. `{ "BUG": "error", "[x]": "none" }`. `"none"` hides a tag completely. |
| `highlight` | `todo-tree.highlights.defaultHighlight.type` | The range covered by each diagnostic: `"tag"`, `"text"` (tag and text up to any comment terminator) or `"line"` (to the end of the line). |
| `diagnostics` | n/a | Set to `false` to only use symbol search, with no diagnostics. |
| `maxResults` | n/a | Limit on how many TODOs are reported across the whole project. `0` means no limit. |

> [!NOTE]
> Zed's project diagnostics panel only lists **errors and warnings**. That's why tags default to `"warning"`. Tags set to `"information"` or `"hint"` still appear in the editor and in symbol search, but not in the panel. Completed `[x]` tasks default to `"hint"` so they don't fill the panel.

Open files are scanned even if they're git-ignored, as in the VS Code extension's "open files" mode. The include and exclude globs still apply to them.

### Using it in more languages

The language server is registered for most of Zed's built-in languages and popular extension languages; see [`extension.toml`](extension.toml). Zed extensions have to name every language they attach to, so if a language you use is missing, add its name to the `languages` list there.

To turn it off for one language:

```json
{
  "languages": {
    "Markdown": { "language_servers": ["!todo-tree", "..."] }
  }
}
```

## Command line

`todo-tree-ls` can also list TODOs from a terminal, which is handy in CI or for scripts:

```bash
todo-tree-ls --list path/to/project
```

```bash
todo-tree-ls --list . --settings '{"tags":["TODO","NOTE"]}'
```

The output uses the `file:line:column: TAG: text` format.

## Not ported

These VS Code features have no equivalent in Zed's extension API: the custom tree view with its tree, flat and tags-only modes and grouping, custom tag icons, highlight colours, the activity bar badge, the status bar item, and the export command. The diagnostics panel and project symbol search stand in for the tree.

## Development

```text
extension.toml      Zed extension manifest
Cargo.toml, src/    The Zed extension (compiled to wasm32-wasip2 by Zed)
server/             todo-tree-ls: the language server (native binary)
```

```bash
cd server && cargo test
```

```bash
cargo build --target wasm32-wasip2 --release
```

To publish a new server version, bump `server/Cargo.toml`, then tag and push. The release workflow attaches `todo-tree-ls-<target>.tar.gz` (or `.zip` on Windows) for macOS, Linux and Windows on x86_64 and aarch64:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

## Credits

Based on [Todo Tree](https://github.com/Gruntfuggly/todo-tree) by Gruntfuggly. MIT licensed; see [License.txt](License.txt).
