use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
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

    #[cfg(test)]
    fn handle_notification(&mut self, connection: &Connection, notification: Notification) {
        for message in self.process_notification(notification) {
            let _ = connection.sender.send(message);
        }
    }

    fn process_notification(&mut self, notification: Notification) -> Vec<Message> {
        match notification.method.as_str() {
            "textDocument/didOpen" => {
                if let Ok(params) =
                    serde_json::from_value::<DidOpenTextDocumentParams>(notification.params)
                {
                    return self
                        .handle_file_event(params.text_document.uri.as_str(), Event::Open)
                        .into_iter()
                        .collect();
                }
            }
            "textDocument/didSave" => {
                if let Ok(params) =
                    serde_json::from_value::<DidSaveTextDocumentParams>(notification.params)
                {
                    return self
                        .handle_file_event(params.text_document.uri.as_str(), Event::Save)
                        .into_iter()
                        .collect();
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn handle_file_event(&mut self, uri: &str, event: Event) -> Option<Message> {
        let Some(path) = uri_to_path(uri) else {
            return None;
        };
        let resolved = match crate::project::resolve_file(&path, true) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => return None,
            Err(error) => {
                return Some(warning_message(format!(
                    "ferry: {}; run a Ferry task for details",
                    safe_error_summary(&error)
                )));
            }
        };

        let enabled = match event {
            Event::Open => resolved.config.editor.pull_on_open,
            Event::Save => resolved.config.editor.push_on_save,
        };
        if !enabled {
            return None;
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
            return Some(warning_message(format!(
                "ferry: {}: {}; run a Ferry task for details",
                resolved.relative_path,
                safe_error_summary(&error)
            )));
        }
        None
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

pub fn main_loop<O: FileOperations + Send + 'static>(
    connection: Connection,
    mut server: Server<O>,
) -> Result<()> {
    let (work_sender, work_receiver) = mpsc::channel::<Notification>();
    let (outbound_sender, outbound_receiver) = mpsc::channel::<Message>();
    let running = Arc::new(AtomicBool::new(true));
    let worker_running = Arc::clone(&running);

    // Deliberately detached: a transport can block indefinitely. The worker
    // owns no clone of the LSP sender, so it cannot keep the stdio writer alive
    // after the protocol loop exits.
    let _worker = thread::spawn(move || {
        while worker_running.load(Ordering::Acquire) {
            let Ok(notification) = work_receiver.recv() else {
                return;
            };
            if !worker_running.load(Ordering::Acquire) {
                return;
            }
            for message in server.process_notification(notification) {
                if outbound_sender.send(message).is_err() {
                    return;
                }
            }
        }
    });

    let result = protocol_loop(&connection, &work_sender, &outbound_receiver);
    running.store(false, Ordering::Release);
    drop(work_sender);
    result
}

fn protocol_loop(
    connection: &Connection,
    work_sender: &mpsc::Sender<Notification>,
    outbound_receiver: &mpsc::Receiver<Message>,
) -> Result<()> {
    loop {
        while let Ok(message) = outbound_receiver.try_recv() {
            if connection.sender.send(message).is_err() {
                return Ok(());
            }
        }

        match connection.receiver.try_recv() {
            Ok(Message::Request(request)) => {
                if handle_request(connection, request)? {
                    return Ok(());
                }
            }
            Ok(Message::Notification(notification)) => {
                if work_sender.send(notification).is_err() {
                    return Ok(());
                }
            }
            Ok(Message::Response(_)) => {}
            Err(error) if error.is_empty() => thread::sleep(Duration::from_millis(5)),
            Err(_) => return Ok(()),
        }
    }
}

fn handle_request(connection: &Connection, request: Request) -> Result<bool> {
    if request.method == "shutdown" {
        return Ok(connection.handle_shutdown(&request)?);
    }

    let response = Response::new_err(
        request.id,
        ErrorCode::MethodNotFound as i32,
        "unsupported request".to_string(),
    );
    let _ = connection.sender.send(Message::Response(response));
    Ok(false)
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

fn warning_message(message: String) -> Message {
    let notification = Notification::new(
        "window/showMessage".to_string(),
        ShowMessageParams {
            typ: MessageType::WARNING,
            message,
        },
    );
    Message::Notification(notification)
}

fn safe_error_summary(error: &anyhow::Error) -> &'static str {
    for source in error.chain() {
        if let Some(exit) = source.downcast_ref::<crate::error::Exit>() {
            return match exit {
                crate::error::Exit::Conflict(_) => "conflict",
                crate::error::Exit::Config(_) => "configuration error",
                crate::error::Exit::Auth(_) => "connection/authentication error",
            };
        }
    }
    "operation failed"
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::str::FromStr;
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use anyhow::{Result, anyhow};
    use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
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
        SensitiveAuth,
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
                Some(Failure::SensitiveAuth) => Err(crate::error::Exit::Auth(format!(
                    "login rejected for {REVIEW_SECRET}"
                ))
                .into()),
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

        fn set_raw_config(&self, config: &str) {
            fs::write(&self.config_path, config).unwrap();
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

    const REVIEW_SECRET: &str = "REVIEW_SECRET_SENTINEL";

    #[test]
    fn automatic_resolution_config_error_never_discloses_source_text() {
        let fixture = Fixture::new("");
        fixture.set_raw_config(&format!(
            "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\n\
             password = \"{REVIEW_SECRET}\n[paths]\nlocal_root = \".\"\nremote_root = \"/\"\n"
        ));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(calls));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        let warning = warning(&client_connection).expect("expected warning notification");
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("configuration error"));
        assert!(!warning.message.contains(REVIEW_SECRET));
    }

    #[test]
    fn automatic_operation_auth_error_never_discloses_details() {
        let fixture = Fixture::new("");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::failing(calls, Failure::SensitiveAuth));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        let warning = warning(&client_connection).expect("expected warning notification");
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("src/nested/hello world.c"));
        assert!(warning.message.contains("connection/authentication error"));
        assert!(!warning.message.contains(REVIEW_SECRET));
    }

    struct CorruptingConfigOperations;

    impl FileOperations for CorruptingConfigOperations {
        fn pull(
            &mut self,
            config_path: &Path,
            _rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            fs::write(
                config_path,
                format!(
                    "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\n\
                     password = \"{REVIEW_SECRET}\n[paths]\nremote_root = \"/\"\n"
                ),
            )?;
            crate::config::Config::load(config_path)?;
            unreachable!("malformed config must fail")
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    #[test]
    fn automatic_second_config_load_error_never_discloses_source_text() {
        let fixture = Fixture::new("");
        let (server_connection, client_connection) = Connection::memory();
        let mut server = Server::new(CorruptingConfigOperations);

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        let warning = warning(&client_connection).expect("expected warning notification");
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("src/nested/hello world.c"));
        assert!(warning.message.contains("configuration error"));
        assert!(!warning.message.contains(REVIEW_SECRET));
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

    struct SendOperations;

    impl FileOperations for SendOperations {
        fn pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    fn send_shutdown(client: &Connection, id: i32) {
        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(id),
                "shutdown".to_string(),
                (),
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification::new(
                "exit".to_string(),
                (),
            )))
            .unwrap();
    }

    fn response_with_id(message: Message, id: i32) -> Response {
        match message {
            Message::Response(response) if response.id == RequestId::from(id) => response,
            other => panic!("expected response {id}, got {other:?}"),
        }
    }

    #[test]
    fn main_loop_replies_method_not_found_to_unsupported_requests() {
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(SendOperations)));
        client_connection
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(41),
                "workspace/unsupported".to_string(),
                (),
            )))
            .unwrap();

        let unsupported = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .ok();
        send_shutdown(&client_connection, 42);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        loop_thread.join().unwrap().unwrap();

        let unsupported = response_with_id(unsupported.expect("unsupported request response"), 41);
        assert_eq!(
            unsupported.error.expect("JSON-RPC error").code,
            ErrorCode::MethodNotFound as i32
        );
        assert!(response_with_id(shutdown, 42).error.is_none());
    }

    struct BlockingOperations {
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
    }

    impl FileOperations for BlockingOperations {
        fn pull(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            self.started.send(()).unwrap();
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.finished.send(()).unwrap();
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn push(
            &mut self,
            _config_path: &Path,
            rel: &str,
            _force: bool,
        ) -> Result<TransferOutcome> {
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn compile(&mut self, _config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    #[test]
    fn main_loop_shutdown_does_not_wait_for_blocked_file_operation() {
        let fixture = Fixture::new("");
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = BlockingOperations {
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
        };
        let (server_connection, client_connection) = Connection::memory();
        let (loop_done_tx, loop_done_rx) = mpsc::sync_channel(1);
        let loop_thread = thread::spawn(move || {
            let result = main_loop(server_connection, Server::new(operations));
            loop_done_tx.send(()).unwrap();
            result
        });
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull should start");

        send_shutdown(&client_connection, 51);
        let prompt_shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .ok();
        let prompt_loop_exit = loop_done_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let writer_disconnected = prompt_loop_exit
            && client_connection
                .receiver
                .try_recv()
                .expect_err("worker must not retain the LSP sender")
                .is_disconnected();

        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked operation should be released");
        loop_thread.join().unwrap().unwrap();

        let shutdown = response_with_id(
            prompt_shutdown.expect("shutdown response must not wait for file operation"),
            51,
        );
        assert!(shutdown.error.is_none());
        assert!(prompt_loop_exit, "main loop should terminate promptly");
        assert!(writer_disconnected, "worker must not retain the LSP sender");
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
