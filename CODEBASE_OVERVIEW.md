# Todo Tree for Zed: Codebase Overview

This repository holds a port of the Todo Tree VS Code extension to Zed. Zed extensions run as WebAssembly and can't add custom views or text decorations. So the port has two parts:

```
┌──────────────────────────┐   starts / configures   ┌─────────────────────────────┐
│ Zed extension (wasm)     │ ──────────────────────▶ │ todo-tree-ls (native, LSP)  │
│ extension.toml, src/     │                         │ server/                     │
│  - finds/downloads the   │                         │  - walks + scans worktree   │
│    server binary         │   diagnostics, symbols  │  - tracks open buffers      │
│  - forwards lsp settings │ ◀────────────────────── │  - publishes diagnostics    │
│  - formats symbol labels │                         │  - workspace/document syms  │
└──────────────────────────┘                         └─────────────────────────────┘
```

## Zed extension (`extension.toml`, `src/lib.rs`)

- `extension.toml` registers the `todo-tree` language server for a long list of languages. Zed has no wildcard for this.
- `language_server_command` picks the binary in this order: `lsp.todo-tree.binary.path` from settings, then `todo-tree-ls` on `PATH`, then the binary already downloaded, then a download of `todo-tree-ls-<target>.{tar.gz,zip}` from the latest GitHub release.
- `language_server_initialization_options` and `language_server_workspace_configuration` forward `lsp.todo-tree.settings`.
- `label_for_symbol` colours the tag in project symbol search.

## Language server (`server/src`)

| File | Role | Replaces (VS Code version) |
| --- | --- | --- |
| `main.rs` | Entry point: LSP on stdio, or the `--list` command line mode | `extension.js` activation |
| `config.rs` | Settings schema, defaults, tag groups, severities | `config.js`, `package.json` settings |
| `scanner.rs` | Builds the regex (`($TAGS)` expansion), finds tags, sub tags and text, and computes UTF-16 LSP positions | `utils.js` (`getRegexSource`, `extractTag`) |
| `workspace.rs` | Parallel, gitignore-aware directory walk and include/exclude glob filtering | `ripgrep.js`, `searchResults.js` |
| `server.rs` | The LSP main loop: document sync, watched files, configuration changes, debounced rescans in background threads, publishing diagnostics (only changed files), workspace and document symbols, progress | `extension.js`, `tree.js`, `highlights.js` |

### Data flow

1. On `initialize`, settings are read from `initializationOptions` and the workspace folders are recorded.
2. After `initialized`, the server registers a `**/*` file watcher and starts a full scan in a background thread for each root.
3. When the scans finish, the results are merged with any open buffers, which override disk contents, and published as diagnostics.
4. `didOpen`/`didChange` rescan the buffer text. `didSave`/`didClose` and watched-file events rescan the file on disk. New directories and `.gitignore` changes trigger a debounced full rescan.
5. `workspace/symbol` returns every TODO, fuzzy-filtered by the query. `textDocument/documentSymbol` returns the TODOs in one file.

## Tests

`cd server && cargo test` runs unit tests for the config, scanner and walker, plus an end-to-end test that drives the server over an in-memory LSP connection.
