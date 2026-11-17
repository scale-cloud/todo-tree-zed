//! The language server: keeps the TODO index up to date and exposes it to Zed
//! as diagnostics (inline + project diagnostics panel, the replacement for the
//! VS Code tree view), workspace symbols (`project symbols`, searchable list of
//! all TODOs) and document symbols.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{select, unbounded, Receiver, Sender};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{self as notif, Notification as _};
use lsp_types::request::{self as req, Request as _};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentSymbolParams, DocumentSymbolResponse, FileChangeType,
    FileSystemWatcher, GlobPattern, InitializeParams, InitializeResult, Location, LogMessageParams,
    MessageType, NumberOrString, OneOf, Position, ProgressParams, ProgressParamsValue,
    PublishDiagnosticsParams, Range, Registration, RegistrationParams, ServerCapabilities,
    ServerInfo, SymbolInformation, SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Url, WorkDoneProgress,
    WorkDoneProgressBegin, WorkDoneProgressCreateParams, WorkDoneProgressEnd,
    WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use serde_json::Value;

use crate::config::{Config, ScanMode, Severity};
use crate::scanner::{Scanner, Todo};
use crate::workspace::{self, Filter, ScanOutput};

const SOURCE: &str = "todo-tree";
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(750);
const PROGRESS_TOKEN: &str = "todo-tree/scan";

struct ScanDone {
    generation: u64,
    root: PathBuf,
    output: ScanOutput,
}

struct Root {
    path: PathBuf,
    filter: Filter,
    gitignore: Option<Gitignore>,
    /// Files with TODOs, as found on disk.
    disk: workspace::Results,
    /// Every file that the last scan looked at.
    files: HashSet<PathBuf>,
}

pub struct Server {
    conn: Connection,
    config: Config,
    scanner: Scanner,
    roots: Vec<Root>,
    /// Text of documents open in the editor.
    open_docs: HashMap<Url, String>,
    /// What was last published per document, used to only send changes.
    published: HashMap<Url, Vec<Diagnostic>>,
    generation: u64,
    pending_scans: usize,
    rescan_at: Option<Instant>,
    scan_tx: Sender<ScanDone>,
    scan_rx: Receiver<ScanDone>,
    next_request_id: i32,
    supports_watchers: bool,
    supports_progress: bool,
    progress_active: bool,
}

fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    if uri.scheme() == "file" {
        uri.to_file_path().ok()
    } else {
        None
    }
}

fn to_range(todo: &Todo) -> Range {
    Range::new(
        Position::new(todo.start.line, todo.start.character),
        Position::new(todo.end.line, todo.end.character),
    )
}

fn to_lsp_severity(severity: Severity) -> Option<DiagnosticSeverity> {
    match severity {
        Severity::Error => Some(DiagnosticSeverity::ERROR),
        Severity::Warning => Some(DiagnosticSeverity::WARNING),
        Severity::Information => Some(DiagnosticSeverity::INFORMATION),
        Severity::Hint => Some(DiagnosticSeverity::HINT),
        Severity::None => None,
    }
}

/// Case-insensitive subsequence match, like most fuzzy symbol pickers.
fn fuzzy_match(query: &str, candidate: &str) -> bool {
    let candidate = candidate.to_lowercase();
    let mut chars = candidate.chars();
    query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|q| chars.any(|c| c == q))
}

fn build_root_gitignore(root: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    builder.add(root.join(".ignore"));
    builder.add(root.join(".git").join("info").join("exclude"));
    builder.build().ok()
}

impl Server {
    pub fn run(conn: Connection) -> Result<(), String> {
        let (id, params) = conn.initialize_start().map_err(|e| e.to_string())?;
        let params: InitializeParams =
            serde_json::from_value(params).map_err(|e| format!("bad initialize params: {e}"))?;

        let (config, config_error) = params
            .initialization_options
            .as_ref()
            .map(Config::from_value)
            .unwrap_or_else(|| (Config::default(), None));

        let supports_watchers = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|w| w.did_change_watched_files.as_ref())
            .and_then(|d| d.dynamic_registration)
            .unwrap_or(false);
        let supports_progress = params
            .capabilities
            .window
            .as_ref()
            .and_then(|w| w.work_done_progress)
            .unwrap_or(false);

        let mut root_paths: Vec<PathBuf> = params
            .workspace_folders
            .as_ref()
            .map(|folders| folders.iter().filter_map(|f| uri_to_path(&f.uri)).collect())
            .unwrap_or_default();
        #[allow(deprecated)]
        if root_paths.is_empty() {
            if let Some(path) = params.root_uri.as_ref().and_then(uri_to_path) {
                root_paths.push(path);
            } else if let Some(path) = params.root_path.as_ref() {
                root_paths.push(PathBuf::from(path));
            }
        }

        let result = InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "todo-tree-ls".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        };
        conn.initialize_finish(id, serde_json::to_value(result).unwrap())
            .map_err(|e| e.to_string())?;

        let (scanner, scanner_error) = match Scanner::new(&config) {
            Ok(s) => (s, None),
            Err(e) => (Scanner::new(&Config::default()).unwrap(), Some(e)),
        };
        let (scan_tx, scan_rx) = unbounded();
        let mut server = Server {
            conn,
            config,
            scanner,
            roots: Vec::new(),
            open_docs: HashMap::new(),
            published: HashMap::new(),
            generation: 0,
            pending_scans: 0,
            rescan_at: None,
            scan_tx,
            scan_rx,
            next_request_id: 1,
            supports_watchers,
            supports_progress,
            progress_active: false,
        };
        for err in [config_error, scanner_error].into_iter().flatten() {
            server.show_error(&err);
        }
        server.set_roots(root_paths);
        server.main_loop()
    }

    // ---------------------------------------------------------------------
    // Messaging helpers

    fn send(&self, msg: Message) {
        let _ = self.conn.sender.send(msg);
    }

    fn notify<N: notif::Notification>(&self, params: N::Params) {
        self.send(Message::Notification(Notification::new(
            N::METHOD.to_string(),
            params,
        )));
    }

    fn request<R: req::Request>(&mut self, params: R::Params) {
        let id = RequestId::from(self.next_request_id);
        self.next_request_id += 1;
        self.send(Message::Request(Request::new(
            id,
            R::METHOD.to_string(),
            params,
        )));
    }

    fn log(&self, message: &str) {
        self.notify::<notif::LogMessage>(LogMessageParams {
            typ: MessageType::LOG,
            message: message.to_string(),
        });
    }

    fn show_error(&self, message: &str) {
        self.notify::<notif::ShowMessage>(lsp_types::ShowMessageParams {
            typ: MessageType::ERROR,
            message: format!("Todo Tree: {message}"),
        });
        self.log(message);
    }

    // ---------------------------------------------------------------------
    // Main loop

    fn main_loop(&mut self) -> Result<(), String> {
        self.register_watchers();
        self.rescan_all();
        loop {
            let timeout = self
                .rescan_at
                .map(|at| at.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(3600));
            let receiver = self.conn.receiver.clone();
            let scan_rx = self.scan_rx.clone();
            select! {
                recv(receiver) -> msg => {
                    let msg = match msg {
                        Ok(msg) => msg,
                        Err(_) => return Ok(()),
                    };
                    match msg {
                        Message::Request(request) => {
                            if self.conn.handle_shutdown(&request).map_err(|e| e.to_string())? {
                                return Ok(());
                            }
                            self.handle_request(request);
                        }
                        Message::Notification(n) => {
                            if n.method == notif::Exit::METHOD {
                                return Ok(());
                            }
                            self.handle_notification(n);
                        }
                        Message::Response(_) => {}
                    }
                }
                recv(scan_rx) -> done => {
                    if let Ok(done) = done {
                        self.scan_finished(done);
                    }
                }
                default(timeout) => {
                    if self.rescan_at.map(|at| Instant::now() >= at).unwrap_or(false) {
                        self.rescan_at = None;
                        self.rescan_all();
                    }
                }
            }
        }
    }

    fn handle_request(&mut self, request: Request) {
        let id = request.id.clone();
        let response = match request.method.as_str() {
            req::WorkspaceSymbolRequest::METHOD => {
                match serde_json::from_value::<WorkspaceSymbolParams>(request.params) {
                    Ok(params) => Response::new_ok(
                        id,
                        WorkspaceSymbolResponse::Flat(self.workspace_symbols(&params.query)),
                    ),
                    Err(e) => invalid_params(id, e),
                }
            }
            req::DocumentSymbolRequest::METHOD => {
                match serde_json::from_value::<DocumentSymbolParams>(request.params) {
                    Ok(params) => Response::new_ok(
                        id,
                        DocumentSymbolResponse::Flat(
                            self.document_symbols(&params.text_document.uri),
                        ),
                    ),
                    Err(e) => invalid_params(id, e),
                }
            }
            _ => Response::new_err(
                id,
                lsp_server::ErrorCode::MethodNotFound as i32,
                format!("unhandled method {}", request.method),
            ),
        };
        self.send(Message::Response(response));
    }

    fn handle_notification(&mut self, n: Notification) {
        match n.method.as_str() {
            notif::DidOpenTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) {
                    self.open_docs
                        .insert(p.text_document.uri.clone(), p.text_document.text);
                    self.publish_uri(&p.text_document.uri);
                }
            }
            notif::DidChangeTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params) {
                    // Full sync: the last change holds the whole document.
                    if let Some(change) = p.content_changes.into_iter().last() {
                        self.open_docs
                            .insert(p.text_document.uri.clone(), change.text);
                        self.publish_uri(&p.text_document.uri);
                    }
                }
            }
            notif::DidSaveTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidSaveTextDocumentParams>(n.params) {
                    if let Some(path) = uri_to_path(&p.text_document.uri) {
                        self.rescan_file(&path);
                    }
                }
            }
            notif::DidCloseTextDocument::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) {
                    self.open_docs.remove(&p.text_document.uri);
                    match uri_to_path(&p.text_document.uri) {
                        // Unsaved edits were discarded: go back to what is on disk.
                        Some(path) => self.rescan_file(&path),
                        None => self.publish_uri(&p.text_document.uri),
                    }
                }
            }
            notif::DidChangeConfiguration::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeConfigurationParams>(n.params) {
                    self.update_config(&p.settings);
                }
            }
            notif::DidChangeWatchedFiles::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeWatchedFilesParams>(n.params) {
                    self.files_changed(p);
                }
            }
            notif::DidChangeWorkspaceFolders::METHOD => {
                if let Ok(p) = serde_json::from_value::<DidChangeWorkspaceFoldersParams>(n.params) {
                    let removed: HashSet<PathBuf> = p
                        .event
                        .removed
                        .iter()
                        .filter_map(|f| uri_to_path(&f.uri))
                        .collect();
                    let mut paths: Vec<PathBuf> = self
                        .roots
                        .iter()
                        .map(|r| r.path.clone())
                        .filter(|p| !removed.contains(p))
                        .collect();
                    paths.extend(p.event.added.iter().filter_map(|f| uri_to_path(&f.uri)));
                    self.set_roots(paths);
                    self.rescan_all();
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------------
    // Configuration and roots

    fn update_config(&mut self, settings: &Value) {
        if settings.is_null() {
            return;
        }
        let (config, error) = Config::from_value(settings);
        if let Some(error) = error {
            self.show_error(&error);
            return;
        }
        match Scanner::new(&config) {
            Ok(scanner) => {
                self.scanner = scanner;
                self.config = config;
                let paths = self.roots.iter().map(|r| r.path.clone()).collect();
                self.set_roots(paths);
                self.rescan_all();
            }
            Err(e) => self.show_error(&e),
        }
    }

    fn set_roots(&mut self, paths: Vec<PathBuf>) {
        let mut roots = Vec::new();
        for path in paths {
            match Filter::new(&path, &self.config) {
                Ok(filter) => roots.push(Root {
                    gitignore: build_root_gitignore(&path),
                    filter,
                    path,
                    disk: Default::default(),
                    files: Default::default(),
                }),
                Err(e) => self.show_error(&e),
            }
        }
        // Keep the most specific root first so nested folders win.
        roots.sort_by_key(|r| std::cmp::Reverse(r.path.components().count()));
        self.roots = roots;
    }

    fn root_index(&self, path: &Path) -> Option<usize> {
        self.roots.iter().position(|r| path.starts_with(&r.path))
    }

    fn register_watchers(&mut self) {
        if !self.supports_watchers || self.config.scan_mode == ScanMode::OpenFiles {
            return;
        }
        let options = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*".to_string()),
                kind: None,
            }],
        };
        self.request::<req::RegisterCapability>(RegistrationParams {
            registrations: vec![Registration {
                id: "todo-tree-watcher".to_string(),
                method: notif::DidChangeWatchedFiles::METHOD.to_string(),
                register_options: Some(serde_json::to_value(options).unwrap()),
            }],
        });
    }

    // ---------------------------------------------------------------------
    // Scanning

    fn rescan_all(&mut self) {
        self.generation += 1;
        self.pending_scans = 0;
        if self.config.scan_mode == ScanMode::OpenFiles {
            for root in &mut self.roots {
                root.disk.clear();
                root.files.clear();
            }
            self.publish_all();
            return;
        }
        self.begin_progress();
        for root in &self.roots {
            let tx = self.scan_tx.clone();
            let config = self.config.clone();
            let path = root.path.clone();
            let generation = self.generation;
            self.pending_scans += 1;
            std::thread::spawn(move || {
                let output = match (Scanner::new(&config), Filter::new(&path, &config)) {
                    (Ok(scanner), Ok(filter)) => {
                        workspace::scan_root(&path, &config, &scanner, &filter)
                    }
                    _ => ScanOutput::default(),
                };
                let _ = tx.send(ScanDone {
                    generation,
                    root: path,
                    output,
                });
            });
        }
        if self.pending_scans == 0 {
            self.end_progress();
            self.publish_all();
        }
    }

    fn scan_finished(&mut self, done: ScanDone) {
        if done.generation != self.generation {
            return;
        }
        if let Some(root) = self.roots.iter_mut().find(|r| r.path == done.root) {
            root.disk = done.output.results;
            root.files = done.output.files;
        }
        self.pending_scans = self.pending_scans.saturating_sub(1);
        if self.pending_scans == 0 {
            self.end_progress();
            self.publish_all();
        }
    }

    fn rescan_file(&mut self, path: &Path) {
        if self.config.scan_mode == ScanMode::OpenFiles {
            if let Ok(uri) = Url::from_file_path(path) {
                self.publish_uri(&uri);
            }
            return;
        }
        if let Some(i) = self.root_index(path) {
            let root = &mut self.roots[i];
            let known = root.files.contains(path);
            if known || root.filter.accepts_file(path) {
                let todos = if path.is_file() {
                    workspace::scan_file(path, &root.filter, &self.scanner)
                } else {
                    Vec::new()
                };
                if !path.exists() {
                    root.files.remove(path);
                } else if !known {
                    // A new file. Only track it if it is not ignored by the
                    // root ignore files (nested ignore files are picked up by
                    // the next full scan).
                    let ignored = root
                        .gitignore
                        .as_ref()
                        .map(|g| {
                            self.config.use_gitignore
                                && g.matched_path_or_any_parents(path, false).is_ignore()
                        })
                        .unwrap_or(false);
                    if ignored {
                        if let Ok(uri) = Url::from_file_path(path) {
                            self.publish_uri(&uri);
                        }
                        return;
                    }
                    root.files.insert(path.to_path_buf());
                }
                if todos.is_empty() {
                    root.disk.remove(path);
                } else {
                    root.disk.insert(path.to_path_buf(), todos);
                }
            }
        }
        if let Ok(uri) = Url::from_file_path(path) {
            self.publish_uri(&uri);
        }
    }

    fn files_changed(&mut self, params: DidChangeWatchedFilesParams) {
        let mut full_rescan = false;
        for change in params.changes {
            let Some(path) = uri_to_path(&change.uri) else {
                continue;
            };
            if path.components().any(|c| c.as_os_str() == ".git") {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == ".gitignore" || name == ".ignore" {
                full_rescan = true;
                continue;
            }
            let Some(i) = self.root_index(&path) else {
                continue;
            };
            if change.typ == FileChangeType::DELETED {
                let root = &mut self.roots[i];
                let was_dir = root
                    .files
                    .iter()
                    .any(|f| f.starts_with(&path) && f != &path);
                if was_dir {
                    root.files.retain(|f| !f.starts_with(&path));
                    let removed: Vec<PathBuf> = root
                        .disk
                        .keys()
                        .filter(|f| f.starts_with(&path))
                        .cloned()
                        .collect();
                    for f in &removed {
                        root.disk.remove(f);
                    }
                    for f in removed {
                        if let Ok(uri) = Url::from_file_path(&f) {
                            self.publish_uri(&uri);
                        }
                    }
                    continue;
                }
            }
            if path.is_dir() {
                // A new directory: let the walker apply nested ignore files.
                full_rescan = true;
                continue;
            }
            self.rescan_file(&path);
        }
        if full_rescan {
            self.rescan_at = Some(Instant::now() + RESCAN_DEBOUNCE);
        }
    }

    // ---------------------------------------------------------------------
    // Progress (shown in Zed's status bar while scanning)

    fn begin_progress(&mut self) {
        if !self.supports_progress || self.progress_active {
            return;
        }
        self.progress_active = true;
        let token = NumberOrString::String(PROGRESS_TOKEN.to_string());
        self.request::<req::WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
            token: token.clone(),
        });
        self.notify::<notif::Progress>(ProgressParams {
            token,
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "Scanning for TODOs".to_string(),
                cancellable: Some(false),
                message: None,
                percentage: None,
            })),
        });
    }

    fn end_progress(&mut self) {
        if !self.progress_active {
            return;
        }
        self.progress_active = false;
        let count: usize = self.all_todos().iter().map(|(_, t)| t.len()).sum();
        self.notify::<notif::Progress>(ProgressParams {
            token: NumberOrString::String(PROGRESS_TOKEN.to_string()),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                message: Some(format!("{count} TODOs found")),
            })),
        });
        self.log(&format!("Scan complete: {count} TODOs found"));
    }

    // ---------------------------------------------------------------------
    // Querying the index

    /// Whether an open document should be scanned.
    fn accepts_open_doc(&self, uri: &Url) -> bool {
        match uri_to_path(uri) {
            Some(path) => match self.root_index(&path) {
                Some(i) => self.roots[i].filter.accepts_file(&path),
                None => true,
            },
            None => true,
        }
    }

    /// Tags found in a document: from the editor buffer if open, otherwise from disk.
    fn todos_for(&self, uri: &Url) -> Vec<Todo> {
        let todos = if let Some(text) = self.open_docs.get(uri) {
            if self.accepts_open_doc(uri) {
                self.scanner.scan(text)
            } else {
                Vec::new()
            }
        } else {
            uri_to_path(uri)
                .and_then(|path| {
                    let i = self.root_index(&path)?;
                    self.roots[i].disk.get(&path).cloned()
                })
                .unwrap_or_default()
        };
        todos
            .into_iter()
            .filter(|t| self.config.severity_for(&t.tag) != Severity::None)
            .collect()
    }

    /// All TODOs, sorted by file, honouring `maxResults`.
    fn all_todos(&self) -> Vec<(Url, Vec<Todo>)> {
        let mut uris: Vec<Url> = self
            .roots
            .iter()
            .flat_map(|r| r.disk.keys())
            .filter_map(|p| Url::from_file_path(p).ok())
            .collect();
        uris.extend(self.open_docs.keys().cloned());
        uris.sort();
        uris.dedup();
        let mut remaining = if self.config.max_results == 0 {
            usize::MAX
        } else {
            self.config.max_results
        };
        let mut out = Vec::new();
        for uri in uris {
            if remaining == 0 {
                break;
            }
            let mut todos = self.todos_for(&uri);
            todos.truncate(remaining);
            remaining -= todos.len();
            if !todos.is_empty() {
                out.push((uri, todos));
            }
        }
        out
    }

    fn diagnostics_for(&self, todos: &[Todo]) -> Vec<Diagnostic> {
        if !self.config.diagnostics {
            return Vec::new();
        }
        todos
            .iter()
            .filter_map(|todo| {
                let severity = to_lsp_severity(self.config.severity_for(&todo.tag))?;
                Some(Diagnostic {
                    range: to_range(todo),
                    severity: Some(severity),
                    code: Some(NumberOrString::String(todo.group.clone())),
                    source: Some(SOURCE.to_string()),
                    message: todo.label(),
                    ..Default::default()
                })
            })
            .collect()
    }

    fn publish(&mut self, uri: Url, diagnostics: Vec<Diagnostic>) {
        let unchanged = match self.published.get(&uri) {
            Some(old) => *old == diagnostics,
            None => diagnostics.is_empty(),
        };
        if unchanged {
            return;
        }
        if diagnostics.is_empty() {
            self.published.remove(&uri);
        } else {
            self.published.insert(uri.clone(), diagnostics.clone());
        }
        self.notify::<notif::PublishDiagnostics>(PublishDiagnosticsParams {
            uri,
            diagnostics,
            version: None,
        });
    }

    fn publish_uri(&mut self, uri: &Url) {
        let diagnostics = self.diagnostics_for(&self.todos_for(uri));
        self.publish(uri.clone(), diagnostics);
    }

    fn publish_all(&mut self) {
        let all = self.all_todos();
        let mut seen = HashSet::new();
        for (uri, todos) in all {
            let diagnostics = self.diagnostics_for(&todos);
            seen.insert(uri.clone());
            self.publish(uri, diagnostics);
        }
        let stale: Vec<Url> = self
            .published
            .keys()
            .filter(|u| !seen.contains(*u))
            .cloned()
            .collect();
        for uri in stale {
            self.publish(uri, Vec::new());
        }
    }

    fn relative_path(&self, uri: &Url) -> Option<String> {
        let path = uri_to_path(uri)?;
        let rel = self
            .root_index(&path)
            .and_then(|i| path.strip_prefix(&self.roots[i].path).ok())
            .unwrap_or(&path);
        Some(rel.to_string_lossy().into_owned())
    }

    #[allow(deprecated)]
    fn symbol(&self, uri: &Url, todo: &Todo) -> SymbolInformation {
        SymbolInformation {
            name: todo.label(),
            kind: SymbolKind::STRING,
            tags: None,
            deprecated: None,
            location: Location::new(uri.clone(), to_range(todo)),
            container_name: self.relative_path(uri),
        }
    }

    fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        let mut symbols = Vec::new();
        for (uri, todos) in self.all_todos() {
            for todo in &todos {
                let label = todo.label();
                if query.is_empty() || fuzzy_match(query, &label) {
                    symbols.push(self.symbol(&uri, todo));
                }
            }
        }
        symbols
    }

    fn document_symbols(&self, uri: &Url) -> Vec<SymbolInformation> {
        self.todos_for(uri)
            .iter()
            .map(|todo| self.symbol(uri, todo))
            .collect()
    }
}

fn invalid_params(id: RequestId, e: serde_json::Error) -> Response {
    Response::new_err(
        id,
        lsp_server::ErrorCode::InvalidParams as i32,
        e.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    struct Client {
        conn: Connection,
        next_id: i32,
    }

    impl Client {
        fn request(&mut self, method: &str, params: Value) -> Value {
            let id = RequestId::from(1000 + self.next_id);
            self.next_id += 1;
            self.conn
                .sender
                .send(Message::Request(Request::new(
                    id.clone(),
                    method.to_string(),
                    params,
                )))
                .unwrap();
            loop {
                match self.recv() {
                    Message::Response(r) if r.id == id => return r.response_result.unwrap(),
                    _ => {}
                }
            }
        }

        fn notify(&self, method: &str, params: Value) {
            self.conn
                .sender
                .send(Message::Notification(Notification::new(
                    method.to_string(),
                    params,
                )))
                .unwrap();
        }

        fn recv(&self) -> Message {
            self.conn
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("server did not respond")
        }

        /// Waits for diagnostics for `uri` and returns their messages.
        fn diagnostics(&self, uri: &Url) -> Vec<String> {
            loop {
                if let Message::Notification(n) = self.recv() {
                    if n.method == notif::PublishDiagnostics::METHOD {
                        let p: PublishDiagnosticsParams = serde_json::from_value(n.params).unwrap();
                        if &p.uri == uri {
                            return p.diagnostics.into_iter().map(|d| d.message).collect();
                        }
                    }
                }
            }
        }
    }

    fn start(root: &Path, options: Value) -> Client {
        let (server_conn, client_conn) = Connection::memory();
        std::thread::spawn(move || Server::run(server_conn).unwrap());
        let mut client = Client {
            conn: client_conn,
            next_id: 0,
        };
        let root_uri = Url::from_file_path(root).unwrap();
        client.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {},
                "initializationOptions": options,
            }),
        );
        client.notify("initialized", json!({}));
        client
    }

    #[test]
    fn end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(
            root.join("a.rs"),
            "// TODO: first\nfn x() {} // FIXME second\n",
        )
        .unwrap();
        fs::write(root.join("b.md"), "- [ ] task\n").unwrap();
        let a = Url::from_file_path(root.join("a.rs")).unwrap();
        let b = Url::from_file_path(root.join("b.md")).unwrap();

        let mut client = start(&root, json!({ "tagSeverity": { "[ ]": "none" } }));
        assert_eq!(client.diagnostics(&a), vec!["TODO: first", "FIXME: second"]);

        let symbols = client.request("workspace/symbol", json!({ "query": "fixsec" }));
        let symbols: Vec<SymbolInformation> = serde_json::from_value(symbols).unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "FIXME: second");
        assert_eq!(symbols[0].container_name.as_deref(), Some("a.rs"));

        // "[ ]" has severity none, so b.md never gets diagnostics.
        let symbols = client.request("workspace/symbol", json!({ "query": "" }));
        assert_eq!(symbols.as_array().unwrap().len(), 2);

        // Editing an open buffer updates diagnostics without saving.
        client.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": a, "languageId": "rust", "version": 1, "text": "// TODO: first\n" } }),
        );
        assert_eq!(client.diagnostics(&a), vec!["TODO: first"]);
        client.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": a, "version": 2 }, "contentChanges": [{ "text": "// HACK edited\n" }] }),
        );
        assert_eq!(client.diagnostics(&a), vec!["HACK: edited"]);

        // Closing without saving falls back to the contents on disk.
        client.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": a } }),
        );
        assert_eq!(client.diagnostics(&a), vec!["TODO: first", "FIXME: second"]);

        // Watched file events rescan files on disk.
        fs::write(root.join("a.rs"), "nothing\n").unwrap();
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": a, "type": 2 }] }),
        );
        assert!(client.diagnostics(&a).is_empty());

        // Configuration changes trigger a rescan with the new settings.
        client.notify(
            "workspace/didChangeConfiguration",
            json!({ "settings": { "todo-tree": { "tags": ["[ ]"] } } }),
        );
        assert_eq!(client.diagnostics(&b), vec!["[ ]: task"]);
        let _ = b;
    }
}
