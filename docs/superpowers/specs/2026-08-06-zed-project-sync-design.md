# Project-Configurable Ferry Integration for Zed

Date: 2026-08-06  
Status: Approved

## Goal

Extend Ferry's existing Zed integration so each Ferry project can pull safely
when Zed opens a file, optionally push when Zed saves it, and expose explicit
current-file Ferry actions inside Zed.

The integration must preserve local and remote data on conflicts. Automatic
operations must never use force.

## Existing Context

Ferry already provides:

- a project-local .ferry.toml configuration file;
- a ferry-lsp process launched by the Ferry Zed extension;
- a textDocument/didOpen handler that currently performs a forced pull;
- example project tasks for pull, push, delete, status, and full sync; and
- a ferry cc command for remote compile checks.

Zed extensions cannot add arbitrary editor context-menu entries. Language
servers can provide Code Actions, which Zed exposes through its lightbulb menu
and the editor: toggle code actions command. Project tasks remain available
through Zed's Task Picker and keybindings.

## Project Configuration

Add an optional editor section to .ferry.toml:

~~~toml
[editor]
pull_on_open = true
push_on_save = false
~~~

Defaults when the section or fields are absent:

- pull_on_open defaults to true, preserving the current extension workflow.
- push_on_save defaults to false, preventing an upgrade from unexpectedly
  deploying local saves to the remote server.

The nearest ancestor .ferry.toml remains the project boundary. Files outside a
Ferry project are ignored.

## Architecture

The existing Ferry Zed extension remains a thin launcher for ferry-lsp.
ferry-lsp owns editor-event handling and current-file Code Actions. The Ferry
library continues to own configuration, path validation, FTP synchronization,
conflict detection, state updates, and compile checks.

The LSP advertises:

- open/close synchronization so it receives textDocument/didOpen;
- save synchronization so it receives textDocument/didSave;
- Code Action support for discoverability; and
- execute-command support for the selected Ferry action.

Operations are processed serially by the LSP event loop. This keeps open and
save operations for a worktree ordered without blocking Zed's UI.

## Event Flow

### Open

1. Zed opens a C or header buffer and sends textDocument/didOpen.
2. ferry-lsp resolves the file URI, finds the nearest Ferry project, and loads
   that project's configuration.
3. If editor.pull_on_open is false, the event is ignored.
4. Otherwise Ferry performs a normal pull for that relative file. It does not
   pass force.
5. If the disk file changes, Zed's file watcher reloads the buffer.

The LSP approach cannot update the file before Zed performs its initial read,
so a brief flash of stale local content can occur. Unsaved or non-file buffers
are ignored.

### Save

1. Zed writes the buffer locally and sends textDocument/didSave.
2. ferry-lsp resolves the Ferry project and loads its configuration.
3. If editor.push_on_save is false, the event is ignored.
4. Otherwise Ferry performs a normal push for that relative file. It does not
   pass force.

A first save of a new file can create the corresponding remote file when
push-on-save is enabled. With push-on-save disabled, the user deploys it with a
manual Push action.

## User-Started Actions

For files served by ferry-lsp, Code Actions provide:

- Ferry: Pull
- Ferry: Push
- Ferry: Compile-check

Pull and Push reuse the same conflict-safe library operations as the automatic
flow. Compile-check reuses Ferry's UDP compile client.

Project .zed/tasks.json also exposes current-file Pull, Push, and Compile-check
tasks, plus Status and broader synchronization tasks. Tasks provide a terminal
view for detailed output and remain usable when a user prefers the Task Picker
or a keybinding.

Destructive deletion is not promoted as a Code Action. If retained in the
example tasks, it must remain clearly labeled as destructive.

## Compile-Check Refactor

The current cc command prints output and calls process exit from command code,
which is unsuitable for reuse inside a long-running language server.

Refactor compile checking into a library operation that returns structured
per-file results and diagnostics. The CLI adapter formats those results and
chooses its exit code. ferry-lsp consumes the same results and shows a concise
OK/FAIL message with diagnostics. The task form continues to provide complete
terminal output.

## Success and Error Feedback

- Successful automatic pulls and pushes are silent.
- Successful manual actions show a concise confirmation.
- A conflict shows a Zed warning containing the affected path and leaves both
  local and remote content unchanged.
- Configuration, authentication, transport, or path failures show a Zed
  warning and never block normal editor use.
- Compile failures are reported as failures, not transport errors, and include
  server diagnostics.
- No automatic or manual action introduced by this feature uses force.

## Change Surface

The implementation is expected to update:

- src/config.rs for the editor configuration and safe defaults;
- src/bin/ferry-lsp.rs for LSP capabilities, open/save dispatch, Code Actions,
  commands, feedback, and test seams;
- src/commands/cc.rs and related UDP types for structured compile results;
- examples/tasks.json for the complete current-file workflow;
- README.md and extensions/ferry/README.md for installation, behavior, and
  configuration; and
- extension metadata if its description or version needs updating.

After Ferry is updated, the 3S project receives only project-specific wiring:
an editor section in .ferry.toml and a .zed/tasks.json file. Credentials and
their values must never be copied into documentation, tests, logs, or commits.

## Verification

Automated verification covers:

- parsing explicit editor settings;
- defaults of pull_on_open=true and push_on_save=false;
- didOpen enabled and disabled paths;
- proof that didOpen calls a non-forced pull;
- didSave enabled and disabled paths;
- proof that didSave calls a non-forced push;
- non-file buffers and files outside Ferry projects;
- Code Action discovery and command dispatch;
- success, conflict, and generic-error notifications;
- structured compile results and CLI exit behavior; and
- the existing Ferry test suite, formatting, and linting.

Tests should use injected or fake operations for LSP behavior and must not
contact the live 3S server.

Deployment verification:

1. Build and test Ferry.
2. Install ferry and ferry-lsp from the updated repository.
3. Install or reload the Ferry development extension in Zed.
4. Add the approved project settings and tasks to the 3S project.
5. Smoke-test pull-on-open with a non-conflicting file.
6. Confirm save does not push when push_on_save is absent or false.
7. Enable push_on_save only for a controlled test project or disposable file,
   then verify the save event.
8. Confirm Code Actions and Task Picker entries target the active file.

## Non-Goals

- Forking Zed or adding native arbitrary context-menu entries.
- Forcing either side through a synchronization conflict.
- Auto-compiling every save.
- Changing Ferry's credential storage or transport protocol.
- Extending the Zed language attachment beyond its current C/header scope as
  part of this feature.
