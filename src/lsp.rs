use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, Command,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, ExecuteCommandOptions,
    ExecuteCommandParams, MessageType, ServerCapabilities, ShowMessageParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, WorkDoneProgressOptions,
};

use crate::commands::ExecutionMode;
use crate::commands::cc::{self, FileCheckResult};
use crate::commands::file_transfer::TransferOutcome;

pub const PULL_COMMAND: &str = "ferry.pull";
pub const PUSH_COMMAND: &str = "ferry.push";
pub const COMPILE_COMMAND: &str = "ferry.compile";
pub const ACTION_COMMANDS: &[&str] = &[PULL_COMMAND, PUSH_COMMAND, COMPILE_COMMAND];

const CODE_ACTION_METHOD: &str = "textDocument/codeAction";
const EXECUTE_COMMAND_METHOD: &str = "workspace/executeCommand";

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

    fn process_request(&mut self, request: Request) -> Vec<Message> {
        match request.method.as_str() {
            CODE_ACTION_METHOD => vec![Message::Response(code_actions(request))],
            EXECUTE_COMMAND_METHOD => self.execute_command(request),
            _ => vec![Message::Response(Response::new_err(
                request.id,
                ErrorCode::MethodNotFound as i32,
                "unsupported request".to_string(),
            ))],
        }
    }

    fn execute_command(&mut self, request: Request) -> Vec<Message> {
        let id = request.id;
        let params = match serde_json::from_value::<ExecuteCommandParams>(request.params) {
            Ok(params) => params,
            Err(_) => {
                return vec![Message::Response(invalid_params(
                    id,
                    "invalid execute-command parameters",
                ))];
            }
        };
        if !ACTION_COMMANDS.contains(&params.command.as_str()) || params.arguments.len() != 1 {
            return vec![Message::Response(invalid_params(
                id,
                "invalid Ferry command or arguments",
            ))];
        }
        let Some(uri) = params.arguments[0].as_str() else {
            return vec![Message::Response(invalid_params(
                id,
                "expected one file URI argument",
            ))];
        };
        let Some(path) = uri_to_path(uri) else {
            return vec![Message::Response(invalid_params(
                id,
                "expected one file URI argument",
            ))];
        };
        let resolved = match crate::project::resolve_file(&path, true) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                return vec![Message::Response(invalid_params(
                    id,
                    "file is outside a Ferry project",
                ))];
            }
            Err(error) => {
                return operation_response(
                    id,
                    warning_message(format!(
                        "ferry: {}; run a Ferry task for details",
                        safe_error_summary(&error)
                    )),
                );
            }
        };
        let relative_path = resolved.relative_path;
        let feedback = match params.command.as_str() {
            PULL_COMMAND => transfer_feedback(
                &relative_path,
                self.operations.pull(
                    &resolved.config_path,
                    &relative_path,
                    /* force = */ false,
                ),
            ),
            PUSH_COMMAND => transfer_feedback(
                &relative_path,
                self.operations.push(
                    &resolved.config_path,
                    &relative_path,
                    /* force = */ false,
                ),
            ),
            COMPILE_COMMAND => compile_feedback(
                &relative_path,
                self.operations
                    .compile(&resolved.config_path, &relative_path),
            ),
            _ => unreachable!("command was validated above"),
        };
        operation_response(id, feedback)
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

fn code_actions(request: Request) -> Response {
    let id = request.id;
    let params = match serde_json::from_value::<CodeActionParams>(request.params) {
        Ok(params) => params,
        Err(_) => return invalid_params(id, "invalid code-action parameters"),
    };
    let uri = params.text_document.uri;
    let actions = uri_to_path(uri.as_str())
        .and_then(|path| crate::project::resolve_file(&path, true).ok().flatten())
        .map(|_| {
            [
                ("Ferry: Pull", PULL_COMMAND),
                ("Ferry: Push", PUSH_COMMAND),
                ("Ferry: Compile-check", COMPILE_COMMAND),
            ]
            .into_iter()
            .map(|(title, command)| {
                CodeActionOrCommand::Command(Command {
                    title: title.to_string(),
                    command: command.to_string(),
                    arguments: Some(vec![serde_json::Value::String(uri.as_str().to_string())]),
                })
            })
            .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Response::new_ok(id, actions)
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
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: ACTION_COMMANDS
                .iter()
                .map(|command| (*command).to_string())
                .collect(),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        ..ServerCapabilities::default()
    }
}

enum Work {
    Notification(Notification),
    Request(Request),
}

pub fn main_loop<O: FileOperations + Send + 'static>(
    connection: Connection,
    mut server: Server<O>,
) -> Result<()> {
    let (work_sender, work_receiver) = mpsc::channel::<Work>();
    let (outbound_sender, outbound_receiver) = mpsc::channel::<Message>();
    let running = Arc::new(AtomicBool::new(true));
    let worker_running = Arc::clone(&running);

    // Deliberately detached: a transport can block indefinitely. The worker
    // owns no clone of the LSP sender, so it cannot keep the stdio writer alive
    // after the protocol loop exits.
    let _worker = thread::spawn(move || {
        while worker_running.load(Ordering::Acquire) {
            let Ok(work) = work_receiver.recv() else {
                return;
            };
            if !worker_running.load(Ordering::Acquire) {
                return;
            }
            let messages = match work {
                Work::Notification(notification) => server.process_notification(notification),
                Work::Request(request) => server.process_request(request),
            };
            for message in messages {
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
    work_sender: &mpsc::Sender<Work>,
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
                if handle_request(connection, work_sender, request)? {
                    return Ok(());
                }
            }
            Ok(Message::Notification(notification)) => {
                if work_sender.send(Work::Notification(notification)).is_err() {
                    return Ok(());
                }
            }
            Ok(Message::Response(_)) => {}
            Err(error) if error.is_empty() => thread::sleep(Duration::from_millis(5)),
            Err(_) => return Ok(()),
        }
    }
}

fn handle_request(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work>,
    request: Request,
) -> Result<bool> {
    if request.method == "shutdown" {
        return Ok(connection.handle_shutdown(&request)?);
    }

    if request.method == CODE_ACTION_METHOD {
        let response = code_actions(request);
        let _ = connection.sender.send(Message::Response(response));
        return Ok(false);
    }

    if request.method == EXECUTE_COMMAND_METHOD {
        return Ok(work_sender.send(Work::Request(request)).is_err());
    }

    let response = Response::new_err(
        request.id,
        ErrorCode::MethodNotFound as i32,
        "unsupported request".to_string(),
    );
    let _ = connection.sender.send(Message::Response(response));
    Ok(false)
}

fn invalid_params(id: lsp_server::RequestId, message: &str) -> Response {
    Response::new_err(id, ErrorCode::InvalidParams as i32, message.to_string())
}

fn operation_response(id: lsp_server::RequestId, feedback: Message) -> Vec<Message> {
    vec![Message::Response(Response::new_ok(id, ())), feedback]
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

fn info_message(message: String) -> Message {
    let notification = Notification::new(
        "window/showMessage".to_string(),
        ShowMessageParams {
            typ: MessageType::INFO,
            message,
        },
    );
    Message::Notification(notification)
}

fn transfer_feedback(relative_path: &str, result: Result<TransferOutcome>) -> Message {
    match result {
        Ok(outcome) => {
            let summary = match outcome.status {
                crate::commands::file_transfer::TransferStatus::Transferred => "transferred",
                crate::commands::file_transfer::TransferStatus::Unchanged => "unchanged",
                crate::commands::file_transfer::TransferStatus::SkippedMissingSource => {
                    "skipped: source missing"
                }
            };
            info_message(format!("ferry: {relative_path}: {summary}"))
        }
        Err(error) => warning_message(format!(
            "ferry: {relative_path}: {}; run a Ferry task for details",
            safe_error_summary(&error)
        )),
    }
}

fn compile_feedback(relative_path: &str, result: Result<FileCheckResult>) -> Message {
    match result {
        Ok(result) => match result.status {
            crate::commands::cc::FileCheckStatus::Passed => {
                info_message(format!("ferry: {relative_path}: compile-check passed"))
            }
            crate::commands::cc::FileCheckStatus::Failed => warning_message(format!(
                "ferry: {relative_path}: compile-check failed: {}",
                result.diagnostics
            )),
            crate::commands::cc::FileCheckStatus::TransportError(_) => warning_message(format!(
                "ferry: {relative_path}: compile-check transport error; run a Ferry task for details"
            )),
        },
        Err(error) => warning_message(format!(
            "ferry: {relative_path}: {}; run a Ferry task for details",
            safe_error_summary(&error)
        )),
    }
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
    use std::time::{Duration, Instant};

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
        Compile {
            config_path: PathBuf,
            rel: String,
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
        compile_status: FileCheckStatus,
        diagnostics: String,
    }

    impl FakeOperations {
        fn successful(calls: Rc<RefCell<Vec<Call>>>) -> Self {
            Self {
                calls,
                failure: None,
                status: TransferStatus::Transferred,
                compile_status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            }
        }

        fn with_status(calls: Rc<RefCell<Vec<Call>>>, status: TransferStatus) -> Self {
            Self {
                calls,
                failure: None,
                status,
                compile_status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            }
        }

        fn failing(calls: Rc<RefCell<Vec<Call>>>, failure: Failure) -> Self {
            Self {
                calls,
                failure: Some(failure),
                status: TransferStatus::Transferred,
                compile_status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            }
        }

        fn compiling(
            calls: Rc<RefCell<Vec<Call>>>,
            status: FileCheckStatus,
            diagnostics: &str,
        ) -> Self {
            Self {
                calls,
                failure: None,
                status: TransferStatus::Transferred,
                compile_status: status,
                diagnostics: diagnostics.to_string(),
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

        fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            self.calls.borrow_mut().push(Call::Compile {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
            });
            match self.failure {
                Some(Failure::Conflict) => {
                    Err(crate::error::Exit::Conflict("changed".into()).into())
                }
                Some(Failure::Generic) => Err(anyhow!("transport unavailable")),
                Some(Failure::SensitiveAuth) => Err(crate::error::Exit::Auth(format!(
                    "login rejected for {REVIEW_SECRET}"
                ))
                .into()),
                None => Ok(FileCheckResult {
                    path: rel.to_string(),
                    status: self.compile_status.clone(),
                    diagnostics: self.diagnostics.clone(),
                }),
            }
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
    fn capabilities_advertise_text_sync_and_exact_ferry_actions() {
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
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![
                        PULL_COMMAND.to_string(),
                        PUSH_COMMAND.to_string(),
                        COMPILE_COMMAND.to_string(),
                    ],
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
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

    fn code_action_request(id: i32, uri: &Uri) -> Request {
        Request::new(
            RequestId::from(id),
            "textDocument/codeAction".to_string(),
            serde_json::json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "context": { "diagnostics": [] }
            }),
        )
    }

    fn request_code_actions(uri: Uri) -> Response {
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(SendOperations)));
        client_connection
            .sender
            .send(Message::Request(code_action_request(61, &uri)))
            .unwrap();

        let response = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("code-action response");
        send_shutdown(&client_connection, 62);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        loop_thread.join().unwrap().unwrap();
        assert!(response_with_id(shutdown, 62).error.is_none());
        response_with_id(response, 61)
    }

    #[test]
    fn code_action_returns_exact_ferry_commands_for_project_file() {
        let fixture = Fixture::new("");
        let uri = fixture.uri();

        let response = request_code_actions(uri.clone());

        assert!(response.error.is_none());
        assert_eq!(
            response.result.unwrap(),
            serde_json::json!([
                {
                    "title": "Ferry: Pull",
                    "command": "ferry.pull",
                    "arguments": [uri]
                },
                {
                    "title": "Ferry: Push",
                    "command": "ferry.push",
                    "arguments": [uri]
                },
                {
                    "title": "Ferry: Compile-check",
                    "command": "ferry.compile",
                    "arguments": [uri]
                }
            ])
        );
    }

    #[test]
    fn code_action_returns_empty_for_non_file_uri() {
        let uri = Uri::from_str("untitled:buffer").unwrap();

        let response = request_code_actions(uri);

        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap(), serde_json::json!([]));
    }

    #[test]
    fn code_action_returns_empty_for_file_outside_project() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("outside.c");
        fs::write(&path, "").unwrap();
        let uri = Uri::from_str(&format!("file://{}", path.display())).unwrap();

        let response = request_code_actions(uri);

        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap(), serde_json::json!([]));
    }

    #[test]
    fn code_action_rejects_malformed_parameters() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut server = Server::new(FakeOperations::successful(calls));
        let request = Request::new(
            RequestId::from(63),
            CODE_ACTION_METHOD.to_string(),
            serde_json::json!({ "textDocument": {} }),
        );

        let (response, messages) = process_server_request(&mut server, request);

        assert_eq!(
            response.error.unwrap().code,
            ErrorCode::InvalidParams as i32
        );
        assert!(messages.is_empty());
    }

    fn execute_command_request(
        id: i32,
        command: &str,
        arguments: Vec<serde_json::Value>,
    ) -> Request {
        Request::new(
            RequestId::from(id),
            EXECUTE_COMMAND_METHOD.to_string(),
            serde_json::json!({
                "command": command,
                "arguments": arguments
            }),
        )
    }

    fn process_server_request<O: FileOperations>(
        server: &mut Server<O>,
        request: Request,
    ) -> (Response, Vec<ShowMessageParams>) {
        let mut response = None;
        let mut notifications = Vec::new();
        for message in server.process_request(request) {
            match message {
                Message::Response(item) => {
                    assert!(response.replace(item).is_none(), "duplicate response");
                }
                Message::Notification(item) if item.method == "window/showMessage" => {
                    notifications.push(serde_json::from_value(item.params).unwrap());
                }
                other => panic!("unexpected server message: {other:?}"),
            }
        }
        (response.expect("request response"), notifications)
    }

    #[test]
    fn execute_command_invalid_inputs_return_invalid_params_without_operations() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside.c");
        fs::write(&outside, "").unwrap();
        let outside_uri = serde_json::json!(format!("file://{}", outside.display()));
        let requests = vec![
            execute_command_request(70, "ferry.unknown", vec![uri.clone()]),
            execute_command_request(71, PULL_COMMAND, vec![]),
            execute_command_request(72, PUSH_COMMAND, vec![uri.clone(), uri.clone()]),
            execute_command_request(73, COMPILE_COMMAND, vec![serde_json::json!(42)]),
            Request::new(
                RequestId::from(74),
                EXECUTE_COMMAND_METHOD.to_string(),
                serde_json::json!({ "arguments": [uri.clone()] }),
            ),
            execute_command_request(75, PULL_COMMAND, vec![serde_json::json!("untitled:buffer")]),
            execute_command_request(76, PUSH_COMMAND, vec![outside_uri]),
        ];
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        for request in requests {
            let (response, messages) = process_server_request(&mut server, request);
            assert_eq!(
                response.error.expect("InvalidParams response").code,
                ErrorCode::InvalidParams as i32
            );
            assert!(messages.is_empty());
        }
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn execute_command_manual_pull_and_push_run_once_without_force() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        for (id, command) in [(80, PULL_COMMAND), (81, PUSH_COMMAND)] {
            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(id, command, vec![uri.clone()]),
            );
            assert!(response.error.is_none());
            assert_eq!(response.result, Some(serde_json::Value::Null));
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, MessageType::INFO);
        }
        assert_eq!(
            *calls.borrow(),
            vec![
                Call::Pull {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::Push {
                    config_path: fixture.config_path,
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                }
            ]
        );
    }

    #[test]
    fn execute_command_transfer_success_feedback_distinguishes_every_outcome() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        for (status, expected) in [
            (TransferStatus::Transferred, "transferred"),
            (TransferStatus::Unchanged, "unchanged"),
            (TransferStatus::SkippedMissingSource, "source missing"),
        ] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut server = Server::new(FakeOperations::with_status(calls, status));

            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(90, PULL_COMMAND, vec![uri.clone()]),
            );

            assert!(response.error.is_none());
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, MessageType::INFO);
            assert!(messages[0].message.contains("src/nested/hello world.c"));
            assert!(messages[0].message.contains(expected));
        }
    }

    #[test]
    fn execute_command_transfer_failures_emit_safe_path_warnings() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        for failure in [Failure::Conflict, Failure::Generic, Failure::SensitiveAuth] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut server = Server::new(FakeOperations::failing(calls, failure));

            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(100, PUSH_COMMAND, vec![uri.clone()]),
            );

            assert!(response.error.is_none());
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, MessageType::WARNING);
            assert!(messages[0].message.contains("src/nested/hello world.c"));
            assert!(!messages[0].message.contains(REVIEW_SECRET));
            assert!(!messages[0].message.contains("transport unavailable"));
        }
    }

    #[test]
    fn execute_command_compile_pass_and_failure_emit_detailed_feedback() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        for (status, diagnostics, expected_type) in [
            (FileCheckStatus::Passed, "", MessageType::INFO),
            (
                FileCheckStatus::Failed,
                "line 9: expected semicolon",
                MessageType::WARNING,
            ),
        ] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let mut server = Server::new(FakeOperations::compiling(
                Rc::clone(&calls),
                status,
                diagnostics,
            ));

            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(110, COMPILE_COMMAND, vec![uri.clone()]),
            );

            assert!(response.error.is_none());
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, expected_type);
            assert!(messages[0].message.contains("src/nested/hello world.c"));
            if !diagnostics.is_empty() {
                assert!(messages[0].message.contains(diagnostics));
            }
            assert_eq!(calls.borrow().len(), 1);
        }
    }

    #[test]
    fn execute_command_compile_transport_cases_emit_safe_warnings() {
        let fixture = Fixture::new("");
        let uri = serde_json::to_value(fixture.uri()).unwrap();
        let cases = [
            FakeOperations::compiling(
                Rc::new(RefCell::new(Vec::new())),
                FileCheckStatus::TransportError(REVIEW_SECRET.to_string()),
                "",
            ),
            FakeOperations::failing(Rc::new(RefCell::new(Vec::new())), Failure::SensitiveAuth),
        ];
        for operations in cases {
            let mut server = Server::new(operations);

            let (response, messages) = process_server_request(
                &mut server,
                execute_command_request(120, COMPILE_COMMAND, vec![uri.clone()]),
            );

            assert!(response.error.is_none());
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].typ, MessageType::WARNING);
            assert!(messages[0].message.contains("src/nested/hello world.c"));
            assert!(!messages[0].message.contains(REVIEW_SECRET));
        }
    }

    struct RecordingSendOperations {
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl FileOperations for RecordingSendOperations {
        fn pull(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::Pull {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::Push {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            Ok(TransferOutcome::new(rel, TransferStatus::Unchanged))
        }

        fn compile(&mut self, config_path: &Path, rel: &str) -> Result<FileCheckResult> {
            self.calls.lock().unwrap().push(Call::Compile {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
            });
            Ok(FileCheckResult {
                path: rel.to_string(),
                status: FileCheckStatus::Passed,
                diagnostics: String::new(),
            })
        }
    }

    fn receive_request_messages(client: &Connection, id: i32, count: usize) -> Vec<Message> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut messages = Vec::new();
        while messages.len() < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for request {id}");
            messages.push(
                client
                    .receiver
                    .recv_timeout(remaining)
                    .unwrap_or_else(|_| panic!("timed out waiting for request {id}")),
            );
        }
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::Response(response) if response.id == RequestId::from(id)))
                .count(),
            1,
            "request must receive exactly one correlated response"
        );
        messages
    }

    #[test]
    fn main_loop_processes_actions_and_commands_with_correlated_feedback() {
        let fixture = Fixture::new("");
        let uri = fixture.uri();
        let uri_value = serde_json::to_value(&uri).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let operations = RecordingSendOperations {
            calls: Arc::clone(&calls),
        };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));

        client_connection
            .sender
            .send(Message::Request(code_action_request(130, &uri)))
            .unwrap();
        let action_messages = receive_request_messages(&client_connection, 130, 1);
        let actions = response_with_id(action_messages.into_iter().next().unwrap(), 130);
        assert!(actions.error.is_none());
        assert_eq!(actions.result.unwrap().as_array().unwrap().len(), 3);

        let mut feedback = Vec::new();
        for (id, command) in [
            (131, PULL_COMMAND),
            (132, PUSH_COMMAND),
            (133, COMPILE_COMMAND),
        ] {
            client_connection
                .sender
                .send(Message::Request(execute_command_request(
                    id,
                    command,
                    vec![uri_value.clone()],
                )))
                .unwrap();
            let messages = receive_request_messages(&client_connection, id, 2);
            for message in messages {
                match message {
                    Message::Response(response) => {
                        assert_eq!(response.id, RequestId::from(id));
                        assert!(response.error.is_none());
                        assert_eq!(response.result, Some(serde_json::Value::Null));
                    }
                    Message::Notification(notification)
                        if notification.method == "window/showMessage" =>
                    {
                        feedback.push(
                            serde_json::from_value::<ShowMessageParams>(notification.params)
                                .unwrap(),
                        );
                    }
                    other => panic!("unexpected protocol message: {other:?}"),
                }
            }
        }

        send_shutdown(&client_connection, 134);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        loop_thread.join().unwrap().unwrap();

        assert!(response_with_id(shutdown, 134).error.is_none());
        assert_eq!(feedback.len(), 3);
        assert!(feedback.iter().all(|message| {
            message.typ == MessageType::INFO && message.message.contains("src/nested/hello world.c")
        }));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Pull {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::Push {
                    config_path: fixture.config_path.clone(),
                    rel: "src/nested/hello world.c".to_string(),
                    force: false,
                },
                Call::Compile {
                    config_path: fixture.config_path,
                    rel: "src/nested/hello world.c".to_string(),
                },
            ]
        );
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
    fn code_action_remains_responsive_while_transfer_worker_is_blocked() {
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
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        client_connection
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pull should start");
        client_connection
            .sender
            .send(Message::Request(code_action_request(141, &fixture.uri())))
            .unwrap();

        let prompt_action = client_connection
            .receiver
            .recv_timeout(Duration::from_millis(500))
            .ok();
        send_shutdown(&client_connection, 142);
        let prompt_shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked operation should be released");
        loop_thread.join().unwrap().unwrap();

        let action = response_with_id(
            prompt_action.expect("code action must not wait for transfer worker"),
            141,
        );
        assert!(action.error.is_none());
        assert_eq!(action.result.unwrap().as_array().unwrap().len(), 3);
        assert!(response_with_id(prompt_shutdown, 142).error.is_none());
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
