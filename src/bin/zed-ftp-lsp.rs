//! Minimal LSP server that pulls files from FTP whenever Zed opens them.
//!
//! On `textDocument/didOpen`, we walk up parent directories from the file to
//! find a `.zed-ftp.toml`, compute the file's path relative to that config,
//! and call `zed_ftp::commands::pull::run` synchronously. Everything else is a
//! no-op.

use std::path::{Path, PathBuf};

use anyhow::Result;
use lsp_server::{Connection, Message, Notification};
use lsp_types::{
    InitializeParams, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
};

fn main() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();

    // We advertise TextDocumentSync::None because we don't need Zed to stream
    // buffer contents to us — we only care about open notifications, which are
    // always delivered regardless of sync mode.
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::NONE)),
        ..ServerCapabilities::default()
    };

    let initialization_params = match connection.initialize(serde_json::to_value(capabilities)?) {
        Ok(it) => it,
        Err(e) => {
            if e.channel_is_disconnected() {
                io_threads.join()?;
            }
            return Err(e.into());
        }
    };
    let _params: InitializeParams = serde_json::from_value(initialization_params)?;

    main_loop(connection)?;
    io_threads.join()?;
    Ok(())
}

fn main_loop(connection: Connection) -> Result<()> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                // Ignore all other requests.
            }
            Message::Notification(n) => {
                if n.method == "textDocument/didOpen" {
                    if let Ok(params) =
                        serde_json::from_value::<lsp_types::DidOpenTextDocumentParams>(n.params)
                    {
                        handle_did_open(&connection, params.text_document.uri.as_str());
                    }
                }
                // Drop everything else silently.
            }
            Message::Response(_) => {
                // We never send requests, so responses are unexpected — drop.
            }
        }
    }
    Ok(())
}

fn handle_did_open(connection: &Connection, uri_str: &str) {
    let Some(path) = uri_to_path(uri_str) else {
        return;
    };
    let Some(config) = find_config(&path) else {
        return;
    };
    // find_config always returns a path with a parent (the directory that
    // contained the config file), but be defensive anyway.
    let Some(root) = config.parent() else {
        return;
    };
    let rel = match path.strip_prefix(root) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => return,
    };

    // force=true: LSP-triggered auto-pull is an opt-in "always give me the
    // remote version" gesture. Users who have locally-modified files they
    // don't want overwritten should not install the LSP extension.
    if let Err(e) = zed_ftp::commands::pull::run(&config, &[rel.clone()], true) {
        show_warning(connection, format!("zed-ftp pull {rel}: {e:#}"));
    }
}

/// Walk up parent directories from `start` looking for `.zed-ftp.toml`.
/// Returns the path to the config file, or `None` if none is found.
fn find_config(start: &Path) -> Option<PathBuf> {
    let mut current = start.parent()?;
    loop {
        let candidate = current.join(".zed-ftp.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        current = current.parent()?;
    }
}

/// Parse a `file://` URI to a filesystem path, percent-decoding as needed.
/// Returns `None` for non-`file` schemes or malformed input.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let stripped = uri.strip_prefix("file://")?;
    // On unix, the authority (between `file://` and the next `/`) is either
    // empty or `localhost`; we don't try to handle remote file URIs.
    let path_part = if let Some(rest) = stripped.strip_prefix("localhost") {
        rest
    } else {
        stripped
    };
    // Must be an absolute path.
    if !path_part.starts_with('/') {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(path_part)
        .decode_utf8()
        .ok()?;
    Some(PathBuf::from(decoded.as_ref()))
}

fn show_warning(connection: &Connection, msg: String) {
    let _ = connection.sender.send(Message::Notification(Notification {
        method: "window/showMessage".to_string(),
        params: serde_json::json!({
            // MessageType::WARNING = 2 in the LSP spec.
            "type": 2,
            "message": msg,
        }),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn find_config_finds_nested_config() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let config = root.join(".zed-ftp.toml");
        fs::write(&config, "").unwrap();

        let nested_dir = root.join("a/b/c");
        fs::create_dir_all(&nested_dir).unwrap();
        let file = nested_dir.join("foo.c");
        fs::write(&file, "").unwrap();

        let found = find_config(&file).expect("should find config");
        // Canonicalize both sides so /tmp -> /private/tmp differences on
        // macOS or symlinked temp dirs don't trip the equality check.
        assert_eq!(
            fs::canonicalize(&found).unwrap(),
            fs::canonicalize(&config).unwrap()
        );
    }

    #[test]
    fn find_config_returns_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        let nested_dir = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested_dir).unwrap();
        let file = nested_dir.join("foo.c");
        fs::write(&file, "").unwrap();

        assert!(find_config(&file).is_none());
    }

    #[test]
    fn uri_to_path_roundtrip_simple() {
        let path = uri_to_path("file:///home/user/foo.c").unwrap();
        assert_eq!(path, PathBuf::from("/home/user/foo.c"));
    }

    #[test]
    fn uri_to_path_percent_decodes_spaces() {
        let path = uri_to_path("file:///home/user/my%20file.c").unwrap();
        assert_eq!(path, PathBuf::from("/home/user/my file.c"));
    }

    #[test]
    fn uri_to_path_rejects_non_file_scheme() {
        assert!(uri_to_path("http://example.com/foo").is_none());
    }

    #[test]
    fn uri_to_path_handles_localhost_authority() {
        let path = uri_to_path("file://localhost/home/user/foo.c").unwrap();
        assert_eq!(path, PathBuf::from("/home/user/foo.c"));
    }
}
