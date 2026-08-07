mod diff;
mod document_state;

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::{
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, Command,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, ExecuteCommandOptions, ExecuteCommandParams, MessageType,
    ServerCapabilities, ShowMessageParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, WorkDoneProgressOptions,
};

use crate::commands::ExecutionMode;
use crate::commands::cc::{self, FileCheckResult};
use crate::commands::file_transfer::TransferOutcome;
use document_state::{DocumentTracker, OperationGuard};

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

#[derive(Clone)]
struct ShutdownBoundary {
    shutdown: Arc<dyn Fn() -> Result<()> + Send + Sync>,
}

impl ShutdownBoundary {
    fn noop() -> Self {
        Self {
            shutdown: Arc::new(|| Ok(())),
        }
    }

    fn shutdown(&self) -> Result<()> {
        (self.shutdown)()
    }
}

pub struct Server<O: FileOperations> {
    operations: O,
    shutdown: ShutdownBoundary,
}

impl<O: FileOperations> Server<O> {
    pub fn new(operations: O) -> Self {
        Self {
            operations,
            shutdown: ShutdownBoundary::noop(),
        }
    }

    #[cfg(test)]
    fn with_shutdown<F>(operations: O, shutdown: F) -> Self
    where
        F: Fn() -> Result<()> + Send + Sync + 'static,
    {
        Self {
            operations,
            shutdown: ShutdownBoundary {
                shutdown: Arc::new(shutdown),
            },
        }
    }

    fn shutdown_handle(&self) -> ShutdownBoundary {
        self.shutdown.clone()
    }

    #[cfg(test)]
    fn handle_notification(&mut self, connection: &Connection, notification: Notification) {
        let message = match notification.method.as_str() {
            "textDocument/didOpen" => {
                serde_json::from_value::<DidOpenTextDocumentParams>(notification.params)
                    .ok()
                    .and_then(|params| {
                        let path = uri_to_path(params.text_document.uri.as_str())?;
                        let mut tracker = DocumentTracker::default();
                        tracker
                            .open(path.clone(), &params.text_document.text)
                            .ok()?;
                        let guard = tracker.begin_clean_operation(&path).ok();
                        self.handle_file_event(&path, Event::Open, guard)
                    })
            }
            "textDocument/didSave" => {
                serde_json::from_value::<DidSaveTextDocumentParams>(notification.params)
                    .ok()
                    .and_then(|params| {
                        let path = uri_to_path(params.text_document.uri.as_str())?;
                        self.handle_file_event(&path, Event::Save, None)
                    })
            }
            _ => None,
        };
        if let Some(message) = message {
            let _ = connection.sender.send(message);
        }
    }

    #[cfg(test)]
    fn process_request(&mut self, request: Request) -> Vec<Message> {
        match request.method.as_str() {
            CODE_ACTION_METHOD => vec![Message::Response(code_actions(request))],
            EXECUTE_COMMAND_METHOD => match prepare_execute_command(request) {
                PreparedRequest::Ready { id, command } => {
                    vec![
                        Message::Response(Response::new_ok(id, ())),
                        self.process_command(command, None),
                    ]
                }
                PreparedRequest::Immediate(messages) => messages,
            },
            _ => vec![Message::Response(Response::new_err(
                request.id,
                ErrorCode::MethodNotFound as i32,
                "unsupported request".to_string(),
            ))],
        }
    }

    fn process_command(
        &mut self,
        command: PreparedCommand,
        guard: Option<OperationGuard>,
    ) -> Message {
        let resolved = match crate::project::resolve_file(&command.absolute_path, true) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                return warning_message(format!(
                    "ferry: {}: file is no longer in a Ferry project",
                    command.initial_relative_path
                ));
            }
            Err(error) => {
                return warning_message(format!(
                    "ferry: {}: {}; run a Ferry task for details",
                    command.initial_relative_path,
                    safe_error_summary(&error)
                ));
            }
        };
        if matches!(command.action, ActionCommand::Pull)
            && guard.is_some_and(|guard| !guard.try_claim())
        {
            return save_first_warning(&resolved.relative_path);
        }
        match command.action {
            ActionCommand::Pull => transfer_feedback(
                &resolved.relative_path,
                self.operations
                    .pull(&resolved.config_path, &resolved.relative_path, false),
            ),
            ActionCommand::Push => transfer_feedback(
                &resolved.relative_path,
                self.operations
                    .push(&resolved.config_path, &resolved.relative_path, false),
            ),
            ActionCommand::Compile => compile_feedback(
                &resolved.relative_path,
                self.operations
                    .compile(&resolved.config_path, &resolved.relative_path),
            ),
        }
    }

    fn handle_file_event(
        &mut self,
        path: &Path,
        event: Event,
        guard: Option<OperationGuard>,
    ) -> Option<Message> {
        let resolved = match crate::project::resolve_file(path, true) {
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
            Event::Open => {
                let Some(guard) = guard else {
                    return Some(save_first_warning(&resolved.relative_path));
                };
                if !guard.try_claim() {
                    return Some(save_first_warning(&resolved.relative_path));
                }
                self.operations
                    .pull(&resolved.config_path, &resolved.relative_path, false)
            }
            Event::Save => {
                self.operations
                    .push(&resolved.config_path, &resolved.relative_path, false)
            }
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
enum ActionCommand {
    Pull,
    Push,
    Compile,
}

struct PreparedCommand {
    action: ActionCommand,
    absolute_path: PathBuf,
    initial_relative_path: String,
}

enum PreparedRequest {
    Ready {
        id: lsp_server::RequestId,
        command: PreparedCommand,
    },
    Immediate(Vec<Message>),
}

fn prepare_execute_command(request: Request) -> PreparedRequest {
    let id = request.id;
    let params = match serde_json::from_value::<ExecuteCommandParams>(request.params) {
        Ok(params) => params,
        Err(_) => {
            return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
                id,
                "invalid execute-command parameters",
            ))]);
        }
    };
    let action = match params.command.as_str() {
        PULL_COMMAND => ActionCommand::Pull,
        PUSH_COMMAND => ActionCommand::Push,
        COMPILE_COMMAND => ActionCommand::Compile,
        _ => {
            return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
                id,
                "invalid Ferry command or arguments",
            ))]);
        }
    };
    if params.arguments.len() != 1 {
        return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
            id,
            "invalid Ferry command or arguments",
        ))]);
    }
    let Some(uri) = params.arguments[0].as_str() else {
        return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
            id,
            "expected one file URI argument",
        ))]);
    };
    let Some(path) = uri_to_path(uri) else {
        return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
            id,
            "expected one file URI argument",
        ))]);
    };
    let resolved = match crate::project::resolve_file(&path, true) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => {
            return PreparedRequest::Immediate(vec![Message::Response(invalid_params(
                id,
                "file is outside a Ferry project",
            ))]);
        }
        Err(error) => {
            return PreparedRequest::Immediate(operation_response(
                id,
                warning_message(format!(
                    "ferry: {}; run a Ferry task for details",
                    safe_error_summary(&error)
                )),
            ));
        }
    };
    PreparedRequest::Ready {
        id,
        command: PreparedCommand {
            action,
            absolute_path: path,
            initial_relative_path: resolved.relative_path,
        },
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
                change: Some(TextDocumentSyncKind::INCREMENTAL),
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
    Open {
        path: PathBuf,
        guard: Option<OperationGuard>,
    },
    Save {
        path: PathBuf,
    },
    Command {
        command: PreparedCommand,
        guard: Option<OperationGuard>,
    },
}

#[derive(Debug)]
struct ShutdownFailures {
    protocol_error: anyhow::Error,
    cleanup_error: anyhow::Error,
}

impl fmt::Display for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Ferry shutdown cleanup also failed: {:#}",
            self.cleanup_error
        )
    }
}

impl Error for ShutdownFailures {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.protocol_error.as_ref())
    }
}

struct Coordinator {
    documents: DocumentTracker,
    running: Arc<AtomicBool>,
    shutdown: ShutdownBoundary,
    shutdown_complete: bool,
    cleanup_error: Option<anyhow::Error>,
}

impl Coordinator {
    fn new(running: Arc<AtomicBool>, shutdown: ShutdownBoundary) -> Self {
        Self {
            documents: DocumentTracker::default(),
            running,
            shutdown,
            shutdown_complete: false,
            cleanup_error: None,
        }
    }

    fn begin_shutdown(&mut self) {
        self.documents.cancel_all();
        self.running.store(false, Ordering::Release);
        if self.shutdown_complete {
            return;
        }
        match self.shutdown.shutdown() {
            Ok(()) => {
                self.shutdown_complete = true;
                self.cleanup_error = None;
            }
            Err(error) => self.cleanup_error = Some(error),
        }
    }

    fn finish(mut self, result: Result<()>) -> Result<()> {
        self.begin_shutdown();
        if self.shutdown_complete {
            return result;
        }
        match (result, self.cleanup_error.take()) {
            (Err(protocol_error), Some(cleanup_error)) => {
                Err(anyhow::Error::new(ShutdownFailures {
                    protocol_error,
                    cleanup_error,
                }))
            }
            (Err(protocol_error), None) => Err(protocol_error),
            (Ok(()), Some(cleanup_error)) => Err(cleanup_error),
            (Ok(()), None) => Err(anyhow::anyhow!("Ferry shutdown cleanup did not complete")),
        }
    }
}

pub fn main_loop<O: FileOperations + Send + 'static>(
    connection: Connection,
    mut server: Server<O>,
) -> Result<()> {
    let (work_sender, work_receiver) = mpsc::channel::<Work>();
    let (outbound_sender, outbound_receiver) = mpsc::channel::<Message>();
    let running = Arc::new(AtomicBool::new(true));
    let worker_running = Arc::clone(&running);
    let shutdown = server.shutdown_handle();

    // Deliberately detached: a transport can block indefinitely. The worker owns no
    // clone of the LSP sender, so it cannot keep the stdio writer alive after the
    // protocol loop exits. Shutdown instead makes queued work inert immediately.
    let _worker = thread::spawn(move || {
        while worker_running.load(Ordering::Acquire) {
            let Ok(work) = work_receiver.recv() else {
                return;
            };
            if !worker_running.load(Ordering::Acquire) {
                return;
            }
            let message = match work {
                Work::Open { path, guard } => server.handle_file_event(&path, Event::Open, guard),
                Work::Save { path } => server.handle_file_event(&path, Event::Save, None),
                Work::Command { command, guard } => Some(server.process_command(command, guard)),
            };
            if !worker_running.load(Ordering::Acquire) {
                return;
            }
            if let Some(message) = message
                && outbound_sender.send(message).is_err()
            {
                return;
            }
        }
    });

    let result = protocol_loop(
        &connection,
        &work_sender,
        &outbound_receiver,
        Arc::clone(&running),
        shutdown,
    );
    drop(work_sender);
    result
}

fn protocol_loop(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work>,
    outbound_receiver: &mpsc::Receiver<Message>,
    running: Arc<AtomicBool>,
    shutdown: ShutdownBoundary,
) -> Result<()> {
    let mut coordinator = Coordinator::new(running, shutdown);
    let result = protocol_loop_inner(connection, work_sender, outbound_receiver, &mut coordinator);
    coordinator.finish(result)
}

fn protocol_loop_inner(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work>,
    outbound_receiver: &mpsc::Receiver<Message>,
    coordinator: &mut Coordinator,
) -> Result<()> {
    loop {
        while let Ok(message) = outbound_receiver.try_recv() {
            if connection.sender.send(message).is_err() {
                return Ok(());
            }
        }

        match connection.receiver.try_recv() {
            Ok(Message::Request(request)) => {
                if handle_request(connection, work_sender, coordinator, request)? {
                    return Ok(());
                }
            }
            Ok(Message::Notification(notification)) => {
                if handle_notification(work_sender, &mut coordinator.documents, notification) {
                    return Ok(());
                }
            }
            Ok(Message::Response(_)) => {}
            Err(error) if error.is_empty() => thread::sleep(Duration::from_millis(5)),
            Err(_) => return Ok(()),
        }
    }
}

fn handle_notification(
    work_sender: &mpsc::Sender<Work>,
    documents: &mut DocumentTracker,
    notification: Notification,
) -> bool {
    match notification.method.as_str() {
        "textDocument/didOpen" => {
            let Ok(params) =
                serde_json::from_value::<DidOpenTextDocumentParams>(notification.params)
            else {
                return false;
            };
            let Some(path) = uri_to_path(params.text_document.uri.as_str()) else {
                return false;
            };
            if documents
                .open(path.clone(), &params.text_document.text)
                .is_err()
            {
                return false;
            }
            let guard = documents.begin_clean_operation(&path).ok();
            work_sender.send(Work::Open { path, guard }).is_err()
        }
        "textDocument/didChange" => {
            if let Ok(params) =
                serde_json::from_value::<DidChangeTextDocumentParams>(notification.params)
                && let Some(path) = uri_to_path(params.text_document.uri.as_str())
            {
                documents.change(&path);
            }
            false
        }
        "textDocument/didSave" => {
            let Ok(params) =
                serde_json::from_value::<DidSaveTextDocumentParams>(notification.params)
            else {
                return false;
            };
            let Some(path) = uri_to_path(params.text_document.uri.as_str()) else {
                return false;
            };
            documents.save(&path);
            work_sender.send(Work::Save { path }).is_err()
        }
        "textDocument/didClose" => {
            if let Ok(params) =
                serde_json::from_value::<DidCloseTextDocumentParams>(notification.params)
                && let Some(path) = uri_to_path(params.text_document.uri.as_str())
            {
                documents.close(&path);
            }
            false
        }
        _ => false,
    }
}

fn handle_request(
    connection: &Connection,
    work_sender: &mpsc::Sender<Work>,
    coordinator: &mut Coordinator,
    request: Request,
) -> Result<bool> {
    if request.method == "shutdown" {
        coordinator.begin_shutdown();
        return Ok(connection.handle_shutdown(&request)?);
    }

    if request.method == CODE_ACTION_METHOD {
        let response = code_actions(request);
        let _ = connection.sender.send(Message::Response(response));
        return Ok(false);
    }

    if request.method == EXECUTE_COMMAND_METHOD {
        match prepare_execute_command(request) {
            PreparedRequest::Ready { id, command } => {
                let guard = if matches!(command.action, ActionCommand::Pull) {
                    match coordinator
                        .documents
                        .begin_clean_operation(&command.absolute_path)
                    {
                        Ok(guard) => Some(guard),
                        Err(_) => {
                            for message in operation_response(
                                id,
                                save_first_warning(&command.initial_relative_path),
                            ) {
                                if connection.sender.send(message).is_err() {
                                    return Ok(true);
                                }
                            }
                            return Ok(false);
                        }
                    }
                } else {
                    None
                };
                let response = if work_sender.send(Work::Command { command, guard }).is_ok() {
                    Response::new_ok(id, ())
                } else {
                    Response::new_err(
                        id,
                        ErrorCode::InternalError as i32,
                        "Ferry operation worker unavailable".to_string(),
                    )
                };
                return Ok(connection.sender.send(Message::Response(response)).is_err());
            }
            PreparedRequest::Immediate(messages) => {
                for message in messages {
                    if connection.sender.send(message).is_err() {
                        return Ok(true);
                    }
                }
                return Ok(false);
            }
        }
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

fn save_first_warning(relative_path: &str) -> Message {
    warning_message(format!("ferry: {relative_path}: save the file and retry"))
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
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Result, anyhow};
    use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
    use lsp_types::{
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DidSaveTextDocumentParams, MessageType, ShowMessageParams, TextDocumentContentChangeEvent,
        TextDocumentIdentifier, TextDocumentItem, Uri, VersionedTextDocumentIdentifier,
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
        did_open_with_text(uri, "int main(void) {}\n")
    }

    fn did_open_with_text(uri: Uri, text: &str) -> Notification {
        Notification::new(
            "textDocument/didOpen".to_string(),
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id: "c".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            },
        )
    }

    fn did_change(uri: Uri) -> Notification {
        Notification::new(
            "textDocument/didChange".to_string(),
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier { uri, version: 2 },
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "changed in editor".to_string(),
                }],
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

    fn did_close(uri: Uri) -> Notification {
        Notification::new(
            "textDocument/didClose".to_string(),
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier::new(uri),
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
    fn automatic_open_with_default_config_calls_nothing() {
        let fixture = Fixture::new("");
        let calls = Rc::new(RefCell::new(Vec::new()));
        let (server_connection, _client_connection) = Connection::memory();
        let mut server = Server::new(FakeOperations::successful(Rc::clone(&calls)));

        server.handle_notification(&server_connection, did_open(fixture.uri()));

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn automatic_open_enabled_pulls_once_without_force() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
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
    fn automatic_editor_settings_are_reloaded_for_each_event() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
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
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
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
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
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
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
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
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
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
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
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

    fn send_shutdown_request(client: &Connection, id: i32) {
        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(id),
                "shutdown".to_string(),
                (),
            )))
            .unwrap();
    }

    fn send_exit(client: &Connection) {
        client
            .sender
            .send(Message::Notification(Notification::new(
                "exit".to_string(),
                (),
            )))
            .unwrap();
    }

    fn send_shutdown(client: &Connection, id: i32) {
        send_shutdown_request(client, id);
        send_exit(client);
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

    fn assert_acknowledged_with_warning(messages: Vec<Message>, id: i32) {
        let mut responses = 0;
        let mut warnings = Vec::new();
        for message in messages {
            match message {
                Message::Response(response) if response.id == RequestId::from(id) => {
                    responses += 1;
                    assert!(response.error.is_none());
                    assert_eq!(response.result, Some(serde_json::Value::Null));
                }
                Message::Notification(notification)
                    if notification.method == "window/showMessage" =>
                {
                    warnings.push(
                        serde_json::from_value::<ShowMessageParams>(notification.params).unwrap(),
                    );
                }
                other => panic!("unexpected protocol message: {other:?}"),
            }
        }
        assert_eq!(responses, 1, "request must be acknowledged exactly once");
        assert_eq!(warnings.len(), 1, "request must emit exactly one warning");
        assert_eq!(warnings[0].typ, MessageType::WARNING);
        assert_eq!(
            warnings[0].message,
            "ferry: src/nested/hello world.c: save the file and retry"
        );
    }

    fn start_recording_loop(
        calls: Arc<Mutex<Vec<Call>>>,
    ) -> (Connection, thread::JoinHandle<Result<()>>) {
        let operations = RecordingSendOperations { calls };
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread =
            thread::spawn(move || main_loop(server_connection, Server::new(operations)));
        (client_connection, loop_thread)
    }

    fn send_pull(client: &Connection, id: i32, uri: &Uri) -> Vec<Message> {
        client
            .sender
            .send(Message::Request(execute_command_request(
                id,
                PULL_COMMAND,
                vec![serde_json::to_value(uri).unwrap()],
            )))
            .unwrap();
        receive_request_messages(client, id, 2)
    }

    fn finish_loop(client: &Connection, loop_thread: thread::JoinHandle<Result<()>>, id: i32) {
        send_shutdown(client, id);
        let shutdown = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        assert!(response_with_id(shutdown, id).error.is_none());
        loop_thread.join().unwrap().unwrap();
    }

    #[test]
    fn dirty_document_matching_did_open_content_permits_pull() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();

        let messages = send_pull(&client, 201, &fixture.uri());

        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Notification(notification) if notification.method == "window/showMessage"
        )));
        assert_eq!(calls.lock().unwrap().len(), 1);
        finish_loop(&client, loop_thread, 202);
    }

    #[test]
    fn dirty_document_differing_did_open_content_refuses_pull_without_operations() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "unsaved editor contents\n",
            )))
            .unwrap();

        let messages = send_pull(&client, 211, &fixture.uri());

        assert_acknowledged_with_warning(messages, 211);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 212);
    }

    #[test]
    fn dirty_document_did_change_refuses_pull_without_operations() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();

        let messages = send_pull(&client, 221, &fixture.uri());

        assert_acknowledged_with_warning(messages, 221);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 222);
    }

    #[test]
    fn dirty_document_did_save_marks_clean_and_permits_a_new_pull() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();
        assert_acknowledged_with_warning(send_pull(&client, 231, &fixture.uri()), 231);
        client
            .sender
            .send(Message::Notification(did_save(fixture.uri())))
            .unwrap();

        let messages = send_pull(&client, 232, &fixture.uri());

        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Notification(notification) if notification.method == "window/showMessage"
        )));
        assert_eq!(calls.lock().unwrap().len(), 1);
        finish_loop(&client, loop_thread, 233);
    }

    #[test]
    fn automatic_restored_dirty_buffer_warns_and_skips_enabled_pull() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "restored unsaved contents\n",
            )))
            .unwrap();

        let warning = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("dirty pull-on-open warning");

        let warning = match warning {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected dirty pull-on-open warning, got {other:?}"),
        };
        assert_eq!(warning.typ, MessageType::WARNING);
        assert_eq!(
            warning.message,
            "ferry: src/nested/hello world.c: save the file and retry"
        );
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 243);
    }

    #[test]
    fn automatic_restored_dirty_buffer_is_silent_when_pull_on_open_is_disabled() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "restored unsaved contents\n",
            )))
            .unwrap();

        client
            .sender
            .send(Message::Request(execute_command_request(
                247,
                COMPILE_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let barrier_messages = receive_request_messages(&client, 247, 2);

        assert!(barrier_messages.iter().all(|message| !matches!(
            message,
            Message::Notification(notification)
                if serde_json::from_value::<ShowMessageParams>(notification.params.clone())
                    .is_ok_and(|params| params.typ == MessageType::WARNING)
        )));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[Call::Compile {
                config_path: fixture.config_path.clone(),
                rel: "src/nested/hello world.c".to_string(),
            }],
            "compile feedback is a FIFO barrier behind the disabled open"
        );
        finish_loop(&client, loop_thread, 248);
    }

    #[test]
    fn dirty_document_malformed_and_non_file_notifications_do_not_clear_dirty_state() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_change(fixture.uri())))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification::new(
                "textDocument/didSave".to_string(),
                serde_json::json!({ "textDocument": {} }),
            )))
            .unwrap();
        for method in [
            "textDocument/didOpen",
            "textDocument/didChange",
            "textDocument/didClose",
        ] {
            client
                .sender
                .send(Message::Notification(Notification::new(
                    method.to_string(),
                    serde_json::json!({ "textDocument": {} }),
                )))
                .unwrap();
        }
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                Uri::from_str("untitled:buffer").unwrap(),
                "not a file",
            )))
            .unwrap();
        let non_file_uri = Uri::from_str("untitled:buffer").unwrap();
        for notification in [
            did_change(non_file_uri.clone()),
            did_save(non_file_uri.clone()),
            did_close(non_file_uri),
        ] {
            client
                .sender
                .send(Message::Notification(notification))
                .unwrap();
        }

        let messages = send_pull(&client, 245, &fixture.uri());

        assert_acknowledged_with_warning(messages, 245);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 246);
    }

    #[test]
    fn dirty_document_did_close_removes_tracking_so_stale_pull_fails_safely() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (client, loop_thread) = start_recording_loop(Arc::clone(&calls));
        client
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client
            .sender
            .send(Message::Notification(did_close(fixture.uri())))
            .unwrap();

        let messages = send_pull(&client, 241, &fixture.uri());

        assert_acknowledged_with_warning(messages, 241);
        assert!(calls.lock().unwrap().is_empty());
        finish_loop(&client, loop_thread, 242);
    }

    #[test]
    fn main_loop_shutdown_cancels_immediately_and_retries_only_incomplete_cleanup() {
        let fixture = Fixture::new("");
        let running = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_shutdown = Arc::clone(&attempts);
        let shutdown = ShutdownBoundary {
            shutdown: Arc::new(move || {
                if attempts_for_shutdown.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    Err(anyhow!("injected cleanup failure"))
                } else {
                    Ok(())
                }
            }),
        };
        let mut coordinator = Coordinator::new(Arc::clone(&running), shutdown);
        coordinator
            .documents
            .open(fixture.file_path.clone(), "int main(void) {}\n")
            .unwrap();
        let guard = coordinator
            .documents
            .begin_clean_operation(&fixture.file_path)
            .unwrap();

        coordinator.begin_shutdown();

        assert!(!running.load(Ordering::Acquire));
        assert!(!guard.try_claim());
        assert!(!coordinator.shutdown_complete);
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);

        coordinator.begin_shutdown();
        assert!(coordinator.shutdown_complete);
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);

        coordinator.begin_shutdown();
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn main_loop_finish_preserves_both_protocol_and_cleanup_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_shutdown = Arc::clone(&attempts);
        let shutdown = ShutdownBoundary {
            shutdown: Arc::new(move || {
                attempts_for_shutdown.fetch_add(1, AtomicOrdering::SeqCst);
                Err(anyhow!("snapshot cleanup failed"))
            }),
        };
        let mut coordinator = Coordinator::new(Arc::new(AtomicBool::new(true)), shutdown);
        coordinator.begin_shutdown();

        let error = coordinator
            .finish(Err(anyhow!("protocol loop failed")))
            .unwrap_err();
        let rendered = format!("{error:#}");
        let combined = error
            .downcast_ref::<ShutdownFailures>()
            .expect("both failures must remain structurally accessible");

        assert_eq!(
            format!("{:#}", combined.protocol_error),
            "protocol loop failed"
        );
        assert_eq!(
            format!("{:#}", combined.cleanup_error),
            "snapshot cleanup failed"
        );
        assert!(rendered.contains("protocol loop failed"), "{rendered}");
        assert!(rendered.contains("snapshot cleanup failed"), "{rendered}");
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn main_loop_failing_shutdown_preserves_protocol_and_cleanup_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_shutdown = Arc::clone(&attempts);
        let server = Server::with_shutdown(SendOperations, move || {
            attempts_for_shutdown.fetch_add(1, AtomicOrdering::SeqCst);
            Err(anyhow!("snapshot cleanup failed"))
        });
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread = thread::spawn(move || main_loop(server_connection, server));
        send_shutdown_request(&client_connection, 249);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown response");
        assert!(response_with_id(shutdown, 249).error.is_none());
        client_connection
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(250),
                "unexpected/duringShutdown".to_string(),
                (),
            )))
            .unwrap();

        let error = loop_thread.join().unwrap().unwrap_err();
        let rendered = format!("{error:#}");

        assert!(
            rendered.contains("unexpected message during shutdown"),
            "{rendered}"
        );
        assert!(rendered.contains("snapshot cleanup failed"), "{rendered}");
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn main_loop_finish_preserves_single_protocol_or_cleanup_error() {
        let protocol_error =
            Coordinator::new(Arc::new(AtomicBool::new(true)), ShutdownBoundary::noop())
                .finish(Err(anyhow!("protocol only")))
                .unwrap_err();
        assert_eq!(format!("{protocol_error:#}"), "protocol only");

        let cleanup_error = Coordinator::new(
            Arc::new(AtomicBool::new(true)),
            ShutdownBoundary {
                shutdown: Arc::new(|| Err(anyhow!("cleanup only"))),
            },
        )
        .finish(Ok(()))
        .unwrap_err();
        assert_eq!(format!("{cleanup_error:#}"), "cleanup only");
    }

    #[test]
    fn main_loop_cleanup_error_does_not_delay_shutdown_response_and_retries_after_exit() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_shutdown = Arc::clone(&attempts);
        let server = Server::with_shutdown(SendOperations, move || {
            if attempts_for_shutdown.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                Err(anyhow!("injected cleanup failure"))
            } else {
                Ok(())
            }
        });
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread = thread::spawn(move || main_loop(server_connection, server));

        send_shutdown_request(&client_connection, 254);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup error must not delay shutdown response");

        assert!(response_with_id(shutdown, 254).error.is_none());
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);

        send_exit(&client_connection);
        loop_thread.join().unwrap().unwrap();
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
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
            .send(Message::Notification(did_open(uri.clone())))
            .unwrap();

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

    struct QueueBlockingOperations {
        calls: Arc<Mutex<Vec<Call>>>,
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
    }

    impl FileOperations for QueueBlockingOperations {
        fn pull(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            let is_first = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(Call::Pull {
                    config_path: config_path.to_path_buf(),
                    rel: rel.to_string(),
                    force,
                });
                calls.len() == 1
            };
            if is_first {
                self.started.send(()).unwrap();
                let (lock, wake) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
                self.finished.send(()).unwrap();
            }
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
        }

        fn push(&mut self, config_path: &Path, rel: &str, force: bool) -> Result<TransferOutcome> {
            self.calls.lock().unwrap().push(Call::Push {
                config_path: config_path.to_path_buf(),
                rel: rel.to_string(),
                force,
            });
            Ok(TransferOutcome::new(rel, TransferStatus::Transferred))
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

    #[test]
    fn dirty_document_shutdown_cancels_queued_pull_and_closes_snapshot_boundary_before_exit() {
        let fixture = Fixture::new("");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = ShutdownBlockingOperations {
            calls: Arc::clone(&calls),
            started: started_tx,
            release: Arc::clone(&release),
            finished: finished_tx,
            stopped: stopped_tx,
        };
        let shutdown_count = Arc::new(AtomicUsize::new(0));
        let shutdown_count_for_loop = Arc::clone(&shutdown_count);
        let (snapshot_closed_tx, snapshot_closed_rx) = mpsc::sync_channel(1);
        let server = Server::with_shutdown(operations, move || {
            shutdown_count_for_loop.fetch_add(1, AtomicOrdering::SeqCst);
            snapshot_closed_tx.send(()).unwrap();
            Ok(())
        });
        let (server_connection, client_connection) = Connection::memory();
        let loop_thread = thread::spawn(move || main_loop(server_connection, server));
        client_connection
            .sender
            .send(Message::Notification(did_open_with_text(
                fixture.uri(),
                "int main(void) {}\n",
            )))
            .unwrap();
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                251,
                PUSH_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_request_messages(&client_connection, 251, 1);
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("preparation blocker should start");
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                252,
                PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        receive_request_messages(&client_connection, 252, 1);

        send_shutdown_request(&client_connection, 253);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response must arrive before exit");

        assert!(response_with_id(shutdown, 253).error.is_none());
        snapshot_closed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot boundary must close during shutdown handshake");
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked preparation should be released");
        stopped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker should exit after the blocked operation returns");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[Call::Push {
                config_path: fixture.config_path.clone(),
                rel: "src/nested/hello world.c".to_string(),
                force: false,
            }],
            "cancelled queued pull must never launch"
        );
        assert!(
            client_connection.receiver.try_recv().is_err(),
            "released worker must not write after shutdown begins"
        );
        assert_eq!(shutdown_count.load(AtomicOrdering::SeqCst), 1);

        send_exit(&client_connection);
        loop_thread.join().unwrap().unwrap();
        assert_eq!(shutdown_count.load(AtomicOrdering::SeqCst), 1);
    }

    struct ShutdownBlockingOperations {
        calls: Arc<Mutex<Vec<Call>>>,
        started: mpsc::SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        finished: mpsc::SyncSender<()>,
        stopped: mpsc::SyncSender<()>,
    }

    impl Drop for ShutdownBlockingOperations {
        fn drop(&mut self) {
            let _ = self.stopped.send(());
        }
    }

    impl FileOperations for ShutdownBlockingOperations {
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
            self.started.send(()).unwrap();
            let (lock, wake) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.finished.send(()).unwrap();
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
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
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
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
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
    fn execute_command_responds_before_blocked_operation_and_shutdown() {
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
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                151,
                PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("manual pull should start");

        let prompt_command = client_connection
            .receiver
            .recv_timeout(Duration::from_millis(500))
            .ok();
        send_shutdown(&client_connection, 152);
        let prompt_shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
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

        let command = response_with_id(
            prompt_command.expect("command response must precede blocked operation"),
            151,
        );
        assert!(command.error.is_none());
        assert_eq!(command.result, Some(serde_json::Value::Null));
        assert!(response_with_id(prompt_shutdown, 152).error.is_none());
        assert!(prompt_loop_exit, "main loop should terminate promptly");
        assert!(writer_disconnected, "worker must not retain the LSP sender");
    }

    #[test]
    fn queued_command_re_resolves_after_project_roots_change() {
        let fixture = Fixture::new("[editor]\npull_on_open = true\n");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let operations = QueueBlockingOperations {
            calls: Arc::clone(&calls),
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
            .expect("automatic pull should block first");
        client_connection
            .sender
            .send(Message::Request(execute_command_request(
                161,
                PULL_COMMAND,
                vec![serde_json::to_value(fixture.uri()).unwrap()],
            )))
            .unwrap();
        let prompt_command = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .ok();

        let other_root = fixture.config_path.parent().unwrap().join("other");
        fs::create_dir(&other_root).unwrap();
        fixture.set_raw_config(
            "[connection]\nhost = \"example.invalid\"\nuser = \"u\"\npassword = \"p\"\n\
             [paths]\nlocal_root = \"other\"\nremote_root = \"/changed\"\n",
        );
        let (lock, wake) = &*release;
        *lock.lock().unwrap() = true;
        wake.notify_all();
        finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked operation should be released");
        let eventual_feedback = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .ok();
        let no_duplicate = client_connection.receiver.try_recv().is_err();
        send_shutdown(&client_connection, 162);
        let shutdown = client_connection
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown response");
        loop_thread.join().unwrap().unwrap();

        let command = response_with_id(prompt_command.expect("prompt command response"), 161);
        assert!(command.error.is_none());
        assert_eq!(command.result, Some(serde_json::Value::Null));
        let warning = match eventual_feedback.expect("safe stale-command warning") {
            Message::Notification(notification) if notification.method == "window/showMessage" => {
                serde_json::from_value::<ShowMessageParams>(notification.params).unwrap()
            }
            other => panic!("expected warning feedback, got {other:?}"),
        };
        assert_eq!(warning.typ, MessageType::WARNING);
        assert!(warning.message.contains("src/nested/hello world.c"));
        assert!(!warning.message.contains("/changed"));
        assert_eq!(calls.lock().unwrap().len(), 1, "stale target must not run");
        assert!(no_duplicate, "command must receive exactly one response");
        assert!(response_with_id(shutdown, 162).error.is_none());
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
