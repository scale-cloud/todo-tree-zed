//! `todo-tree-ls`: the language server behind the Todo Tree Zed extension.
//!
//! Usage:
//!   todo-tree-ls                 Run the language server on stdio (what Zed does).
//!   todo-tree-ls --list [DIR]    Print all TODOs under DIR (default: current dir).
//!   todo-tree-ls --version

mod config;
mod scanner;
mod server;
mod workspace;

use std::path::PathBuf;
use std::process::ExitCode;

fn list(dir: PathBuf, settings: Option<String>) -> Result<(), String> {
    let value = match settings {
        Some(json) => serde_json::from_str(&json).map_err(|e| format!("--settings: {e}"))?,
        None => serde_json::Value::Null,
    };
    let (config, error) = config::Config::from_value(&value);
    if let Some(error) = error {
        return Err(error);
    }
    let scanner = scanner::Scanner::new(&config)?;
    let filter = workspace::Filter::new(&dir, &config)?;
    let output = workspace::scan_root(&dir, &config, &scanner, &filter);
    let mut count = 0;
    for (path, todos) in &output.results {
        let shown = path
            .strip_prefix(&dir)
            .unwrap_or(path)
            .display()
            .to_string();
        for todo in todos {
            if config.severity_for(&todo.tag) == config::Severity::None {
                continue;
            }
            count += 1;
            println!(
                "{}:{}:{}: {}",
                shown,
                todo.start.line + 1,
                todo.start.character + 1,
                todo.label()
            );
        }
    }
    eprintln!("{count} TODOs in {} files", output.results.len());
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut mode_list = false;
    let mut dir = None;
    let mut settings = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("todo-tree-ls {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!(
                    "todo-tree-ls {}\n\n\
                     USAGE:\n  todo-tree-ls                     run the language server on stdio\n  \
                     todo-tree-ls --list [DIR]        print TODOs found under DIR\n\n\
                     OPTIONS:\n  --settings JSON   settings object (same keys as the Zed `lsp.todo-tree.settings`)",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            "--list" => mode_list = true,
            "--settings" => settings = args.next(),
            // Accepted for compatibility with clients that always pass it.
            "--stdio" => {}
            other if mode_list && dir.is_none() => dir = Some(PathBuf::from(other)),
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let result = if mode_list {
        list(dir.unwrap_or_else(|| PathBuf::from(".")), settings)
    } else {
        let (connection, io_threads) = lsp_server::Connection::stdio();
        let result = server::Server::run(connection);
        let _ = io_threads.join();
        result
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("todo-tree-ls: {e}");
            ExitCode::FAILURE
        }
    }
}
