use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Notification};
use lsp_types::{
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, MessageType, ServerCapabilities,
    ShowMessageParams, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions,
};

use crate::commands::ExecutionMode;
use crate::commands::cc::{self, FileCheckResult};
use crate::commands::file_transfer::TransferOutcome;

pub trait FileOperations {
    fn pull(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome>;

    fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome>;

    fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult>;
}

pub struct FerryOperations;

impl FileOperations for FerryOperations {
    fn pull(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
        crate::commands::pull::pull_one(config_path, rel, force, ExecutionMode::Apply)
    }

    fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
        crate::commands::push::push_one(config_path, rel, force, ExecutionMode::Apply)
    }

    fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult> {
        cc::check_files(config_path, &[rel.to_string()])?
            .into_iter()
            .next()
            .context("compile check returned no result")
    }
}

pub struct Server<O: FileOperations> {
    operations: O,
}

impl<O: FileOperations> Server<O> {
    pub fn new(operations: O) -> Self {
        Self { operations }
    }

    fn handle_notification(&mut self, connection: &Connection, notification: Notification) {
        match notification.method.as_str() {
            "textDocument/didOpen" => {
                if let Ok(params) =
                    serde_json::from_value::<DidOpenTextDocumentParams>(notification.params)
                {
                    self.handle_file_event(
                        connection,
                        params.text_document.uri.as_str(),
                        Event::Open,
                    );
                }
            }
            "textDocument/didSave" => {
                if let Ok(params) =
                    serde_json::from_value::<DidSaveTextDocumentParams>(notification.params)
                {
                    self.handle_file_event(
                        connection,
                        params.text_document.uri.as_str(),
                        Event::Save,
                    );
                }
            }
            _ => {}
        }
    }

    fn handle_file_event(&mut self, connection: &Connection, uri: &str, event: Event) {
        let Some(path) = uri_to_path(uri) else {
            return;
        };
        let resolved = match crate::project::resolve_file(&path, true) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => return,
            Err(error) => {
                show_warning(connection, format!("ferry: {}: {error:#}", path.display()));
                return;
            }
        };

        let enabled = match event {
            Event::Open => resolved.config.editor.pull_on_open,
            Event::Save => resolved.config.editor.push_on_save,
        };
        if !enabled {
            return;
        }

        let result = match event {
            Event::Open => self.operations.pull(
                &resolved.config_path,
                &resolved.relative_path,
                /* force = */ false,
            ),
            Event::Save => self.operations.push(
                &resolved.config_path,
                &resolved.relative_path,
                /* force = */ false,
            ),
        };
        if let Err(error) = result {
            show_warning(
                connection,
                format!("ferry: {}: {error:#}", resolved.relative_path),
            );
        }
    }
}

#[derive(Clone, Copy)]
enum Event {
    Open,
    Save,
}

pub fn capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::NONE),
                will_save: None,
                will_save_wait_until: None,
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
            },
        )),
        ..ServerCapabilities::default()
    }
}

pub fn main_loop<O: FileOperations>(connection: Connection, mut server: Server<O>) -> Result<()> {
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    return Ok(());
                }
            }
            Message::Notification(notification) => {
                server.handle_notification(&connection, notification);
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let stripped = uri.strip_prefix("file://")?;
    let path_part = if let Some(rest) = stripped.strip_prefix("localhost") {
        rest
    } else {
        stripped
    };
    if !path_part.starts_with('/') {
        return None;
    }
    let decoded = percent_encoding::percent_decode_str(path_part)
        .decode_utf8()
        .ok()?;
    Some(PathBuf::from(decoded.as_ref()))
}

fn show_warning(connection: &Connection, message: String) {
    let notification = Notification::new(
        "window/showMessage".to_string(),
        ShowMessageParams {
            typ: MessageType::WARNING,
            message,
        },
    );
    let _ = connection.sender.send(Message::Notification(notification));
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::str::FromStr;

    use anyhow::{Result, anyhow};
    use lsp_server::{Connection, Message, Notification};
    use lsp_types::{
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, MessageType, ShowMessageParams,
        TextDocumentIdentifier, TextDocumentItem, Uri,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::commands::cc::{FileCheckResult, FileCheckStatus};
    use crate::commands::file_transfer::{TransferOutcome, TransferStatus};

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Pull {
            config_path: PathBuf,
            rel: String,
            force: bool,
        },
        Push {
            config_path: PathBuf,
            rel: String,
            force: bool,
        },
    }

    #[derive(Clone, Copy)]
    enum Failure {
        Conflict,
        Generic,
    }

    struct FakeOperations {
        calls: Rc<RefCell<Vec<Call>>>,
        failure: Option<Failure>,
        status: TransferStatus,
    }

    impl FakeOperations {
        fn successful(calls: Rc<RefCell<Vec<Call>>>) -> Self {
            Self {
                calls,
                failure: None,
                status: TransferStatus::Transferred,
            }
        }

        fn with_status(calls: Rc<RefCell<Vec<Call>>>, status: TransferStatus) -> Self {
            Self {
                calls,
                failure: None,
                status,
            }
        }

        fn failing(calls: Rc<RefCell<Vec<Call>>>, failure: Failure) -> Self {
            Self {
                calls,
                failure: Some(failure),
                status: TransferStatus::Transferred,
            }
        }

        fn result(&self, rel: &str) -> Result<TransferOutcome> {
            match self.failure {
                Some(Failure::Conflict) => {
                    Err(crate::error::Exit::Conflict("changed".into()).into())
                }
                Some(Failure::Generic) => Err(anyhow!("transport unavailable")),
                None => Ok(TransferOutcome::new(rel, self.status)),
            }
        }
    }

    impl FileOperations for FakeOperations {
        fn pull(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.borrow_mut().push(Call::Pull {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            self.result(rel)
        }

        fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.borrow_mut().push(Call::Push {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            self.result(rel)
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    struct Fixture {
        _temp: TempDir,
        config_path: PathBuf,
        file_path: PathBuf,
    }

    impl Fixture {
        fn new(editor: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let nested = root.join("src/nested");
            fs::create_dir_all(&nested).unwrap();
            let file_path = nested.join("hello world.c");
            fs::write(&file_path, "int main(void) {}\n").unwrap();
            let config_path = root.join(crate::names::CONFIG_FILE);
            let fixture = Self {
                _temp: temp,
                config_path,
                file_path,
            };
            fixture.set_editor(editor);
            fixture
        }

        fn uri(&self) -> Uri {
            let encoded = self.file_path.to_string_lossy().replace(' ', "%20");
            Uri::from_str(&format!("file://{encoded}")).unwrap()
        }

        fn set_editor(&self, editor: &str) {
            fs::write(
                &self.config_path,
                format!(
                    "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\npassword = \"p\"\n\
                     [paths]\nlocal_root = \".\"\nremote_root = \"/project\"\n{editor}"
                ),
            )
            .unwrap();
        }
    }

    fn did_open(uri: Uri) -> Notification {
        Notification::new(
            "textDocument/didOpen".to_string(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: "c".to_string(),
                    version: 1,
                    text: String::new(),
                },
            },
        )
    }

    fn did_save(uri: Uri) -> Notification {
        Notification::new(
            "textDocument/didSave".to_string(),
            DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier::new(uri),
                text: None,
            },
        )
    }

    fn warning(client: &Connection) -> Option<ShowMessageParams> {
        match client.receiver.try_recv().ok()? {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value(notification.params).ok()
            }
            other => panic!("unexpected server message: {other:?}"),
        }
    }

    #[test]
    fn automatic_open_with_default_config_pulls_once_without_force() {
        let fixture = Fixture::new("");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        assert_eq!(
            *calls.borrow(),
            vec![Call::Pull {
                config_path: fixture.config_path,
                rel: "src/nested/hello world.c".to_string(),
                force: false,
            }]
        );
    }

    #[test]
    fn automatic_open_disabled_calls_nothing() {
        let fixture = Fixture::new("[editor]\npull_on_open = false\n");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn automatic_editor_settings_are_reloaded_for_each_event() {
        let fixture = Fixture::new("");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(fixture.uri()));
        fixture.set_editor("[editor]\npull_on_open = false\n");
        server.handle_notification(&server_connection, did_open(fixture.uri()));

        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn automatic_save_with_default_config_calls_nothing() {
        let fixture = Fixture::new("");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_save(fixture.uri()));

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn automatic_save_enabled_pushes_once_without_force() {
        let fixture = Fixture::new("[editor]\npush_on_save = true\n");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_save(fixture.uri()));

        assert_eq!(
            *calls.borrow(),
            vec![Call::Push {
                config_path: fixture.config_path,
                rel: "src/nested/hello world.c".to_string(),
                force: false,
            }]
        );
    }

    #[test]
    fn automatic_non_file_uri_calls_nothing() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));
        let uri = Uri::from_str("untitled:buffer").unwrap();

        server.handle_notification(&server_connection, did_open(uri));

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn automatic_file_outside_project_calls_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outside.c");
        fs::write(&path, "").unwrap();
        let uri = Uri::from_str(&format!("file://{}", path.display())).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(uri));

        assert!(calls.borrow().is_empty());
    }

    fn automatic_failure_emits_warning(failure: Failure) {
        let fixture = Fixture::new("");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::failing(calls, failure));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        let warning = warning(&client_connection).expect("expected warning notification");
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("src/nested/hello world.c"));
    }

    #[test]
    fn automatic_conflict_emits_path_warning() {
        automatic_failure_emits_warning(Failure::Conflict);
    }

    #[test]
    fn automatic_generic_failure_emits_path_warning() {
        automatic_failure_emits_warning(Failure::Generic);
    }

    #[test]
    fn automatic_success_outcomes_are_silent() {
        let fixture = Fixture::new("");
        for status in [
            TransferStatus::Transferred,
            TransferStatus::Unchanged,
            TransferStatus::SkippedMissingSource,
        ] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let (server_connection, client_connection) = Connection::memory();
            let mut server = Server::new(FakeOperations::with_status(calls, status));

            server.handle_notification(&server_connection, did_open(fixture.uri()));

            assert!(client_connection.receiver.try_recv().is_err());
        }
    }

    #[test]
    fn task_six_capabilities_advertise_only_text_synchronization() {
        assert_eq!(
            capabilities(),
            ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::NONE),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    },
                )),
                ..ServerCapabilities::default()
            }
        );
    }

    #[test]
    fn uri_to_path_roundtrip_simple() {
        assert_eq!(
            uri_to_path("file:///home/user/foo.c"),
            Some(PathBuf::from("/home/user/foo.c"))
        );
    }

    #[test]
    fn uri_to_path_percent_decodes_spaces() {
        assert_eq!(
            uri_to_path("file:///home/user/my%20file.c"),
            Some(PathBuf::from("/home/user/my file.c"))
        );
    }

    #[test]
    fn uri_to_path_rejects_non_file_scheme() {
        assert!(uri_to_path("http://example.com/foo").is_none());
    }

    #[test]
    fn uri_to_path_handles_localhost_authority() {
        assert_eq!(
            uri_to_path("file://localhost/home/user/foo.c"),
            Some(PathBuf::from("/home/user/foo.c"))
        );
    }
}
