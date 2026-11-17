//! Zed extension for Todo Tree.
//!
//! Zed extensions cannot add custom tree views or editor decorations, so the
//! work is done by a small language server (`todo-tree-ls`, see `server/`)
//! that scans the project for tags and reports them as diagnostics and
//! symbols. This extension only locates (or downloads) that server, forwards
//! settings to it and formats its symbols.

use std::fs;

use zed_extension_api::{
    self as zed, settings::LspSettings, CodeLabel, CodeLabelSpan, LanguageServerId, Result,
};

const SERVER_NAME: &str = "todo-tree";
const BINARY_NAME: &str = "todo-tree-ls";
const GITHUB_REPO: &str = "JordanRickman/todo-tree";

struct TodoTreeExtension {
    cached_binary_path: Option<String>,
}

fn target_triple() -> Result<(&'static str, zed::DownloadedFileType)> {
    let (os, arch) = zed::current_platform();
    let triple = match (os, arch) {
        (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
        (zed::Os::Mac, zed::Architecture::X8664) => "x86_64-apple-darwin",
        (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
        (zed::Os::Linux, zed::Architecture::X8664) => "x86_64-unknown-linux-gnu",
        (zed::Os::Windows, zed::Architecture::Aarch64) => "aarch64-pc-windows-msvc",
        (zed::Os::Windows, zed::Architecture::X8664) => "x86_64-pc-windows-msvc",
        (os, arch) => return Err(format!("unsupported platform: {os:?} {arch:?}")),
    };
    let file_type = match os {
        zed::Os::Windows => zed::DownloadedFileType::Zip,
        _ => zed::DownloadedFileType::GzipTar,
    };
    Ok((triple, file_type))
}

impl TodoTreeExtension {
    fn binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<String> {
        if let Some(path) = worktree.which(BINARY_NAME) {
            return Ok(path);
        }

        if let Some(path) = &self.cached_binary_path {
            if fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok(path.clone());
            }
        }

        zed::set_language_server_installation_status(
            language_server_id,
            &zed::LanguageServerInstallationStatus::CheckingForUpdate,
        );
        let release = zed::latest_github_release(
            GITHUB_REPO,
            zed::GithubReleaseOptions {
                require_assets: true,
                pre_release: false,
            },
        )?;

        let (triple, file_type) = target_triple()?;
        let extension = match file_type {
            zed::DownloadedFileType::Zip => "zip",
            _ => "tar.gz",
        };
        let asset_name = format!("{BINARY_NAME}-{triple}.{extension}");
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == asset_name)
            .ok_or_else(|| format!("no asset named {asset_name} in release {}", release.version))?;

        let version_dir = format!("{BINARY_NAME}-{}", release.version);
        let exe = match zed::current_platform().0 {
            zed::Os::Windows => format!("{BINARY_NAME}.exe"),
            _ => BINARY_NAME.to_string(),
        };
        let binary_path = format!("{version_dir}/{exe}");

        if !fs::metadata(&binary_path).is_ok_and(|m| m.is_file()) {
            zed::set_language_server_installation_status(
                language_server_id,
                &zed::LanguageServerInstallationStatus::Downloading,
            );
            zed::download_file(&asset.download_url, &version_dir, file_type)
                .map_err(|e| format!("failed to download {asset_name}: {e}"))?;
            zed::make_file_executable(&binary_path)?;

            // Remove older versions.
            if let Ok(entries) = fs::read_dir(".") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with(BINARY_NAME) && name != version_dir.as_str() {
                        fs::remove_dir_all(entry.path()).ok();
                    }
                }
            }
        }

        self.cached_binary_path = Some(binary_path.clone());
        Ok(binary_path)
    }
}

impl zed::Extension for TodoTreeExtension {
    fn new() -> Self {
        TodoTreeExtension {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let binary = LspSettings::for_worktree(SERVER_NAME, worktree)
            .ok()
            .and_then(|s| s.binary);
        let env = binary
            .as_ref()
            .and_then(|b| b.env.clone())
            .map(|env| env.into_iter().collect())
            .unwrap_or_default();
        let args = binary
            .as_ref()
            .and_then(|b| b.arguments.clone())
            .unwrap_or_default();
        let command = match binary.and_then(|b| b.path) {
            Some(path) => path,
            None => self.binary_path(language_server_id, worktree)?,
        };
        Ok(zed::Command { command, args, env })
    }

    fn language_server_initialization_options(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        // `lsp.todo-tree.settings` is the documented place for settings;
        // `initialization_options` is accepted as well.
        let settings = LspSettings::for_worktree(SERVER_NAME, worktree).ok();
        Ok(settings.and_then(|s| s.initialization_options.or(s.settings)))
    }

    fn language_server_workspace_configuration(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        let settings = LspSettings::for_worktree(SERVER_NAME, worktree).ok();
        Ok(settings.and_then(|s| s.settings))
    }

    fn label_for_symbol(
        &self,
        _language_server_id: &LanguageServerId,
        symbol: zed::lsp::Symbol,
    ) -> Option<CodeLabel> {
        // Symbols are named "TAG: text"; colour the tag like a keyword and
        // the rest like a comment.
        let name = symbol.name;
        let (tag, rest) = match name.find(": ") {
            Some(i) => (name[..i].to_string(), name[i..].to_string()),
            None => (name.clone(), String::new()),
        };
        let mut spans = vec![CodeLabelSpan::literal(tag, Some("keyword".to_string()))];
        if !rest.is_empty() {
            spans.push(CodeLabelSpan::literal(rest, Some("comment".to_string())));
        }
        Some(CodeLabel {
            code: String::new(),
            spans,
            filter_range: (0..name.len() as u32).into(),
        })
    }
}

zed::register_extension!(TodoTreeExtension);
