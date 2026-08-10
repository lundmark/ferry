# Zed Scoped File and Directory Sync Design

**Date:** 2026-08-10

**Status:** Approved for implementation planning

## Context

Ferry currently exposes project-wide bidirectional `sync`, path-aware one-way
`pull` and `push`, and five current-file Zed Code Actions. The user also wants
to synchronize an individual file or directory from Zed, including a
top-level directory that exists only on the remote and has never been copied
locally.

Zed's public extension API does not currently allow an extension to add items
to the Project Panel's right-click menu or obtain the selected Project Panel
path. Zed tasks receive active-editor variables rather than the Project Panel
selection. The supported design therefore combines current-editor Code
Actions, terminal-backed Zed tasks, and a Ferry-owned interactive path
browser.

## Goals

- Synchronize exactly one selected file or directory per invocation.
- Make directory synchronization recursive.
- Support local-only, remote-only, and shared paths, including empty
  directories.
- Let a user discover and select a remote-only root directory without first
  creating a local file.
- Expose current-file and current-folder sync through Zed Code Actions and
  tasks.
- Keep bare `ferry sync` backward compatible as project-wide sync.
- Preserve Ferry's existing conflict-safe classification, dry-run behavior,
  exit codes, active ignore rules, and state format.
- Keep all automatic editor behavior opt-in and disabled by default.

## Non-goals

- Custom Project Panel context-menu entries.
- A native graphical Zed picker.
- Selecting multiple paths in one invocation.
- Automatic directory synchronization.
- Deletion propagation.
- A force-sync action or task supplied to Zed.
- Defining or activating the currently unused `sync.include` configuration
  field.
- Replacing Zed's file browser or building a separate GUI.

## Chosen UX

### Current editor context

For documents served by `ferry-lsp`, `Ctrl+.` continues to open Zed's Code
Action menu. The actions appear in this order:

1. `Ferry: Pull`
2. `Ferry: Compare with Remote`
3. `Ferry: Force Pull (overwrite local)`
4. `Ferry: Push`
5. `Ferry: Sync Current File`
6. `Ferry: Sync Current Folder`
7. `Ferry: Compile-check`

`Sync Current File` selects only the saved file represented by the current
document. `Sync Current Folder` selects the current file's parent directory
and recursively processes its subtree. If the current file is directly under
the configured local root, the folder action is equivalent in scope to a
project-wide sync.

Both actions are explicit manual operations. They are visible regardless of
`editor.pull_on_open` and `editor.push_on_save`; those settings continue to
control only automatic open/save behavior and continue to default to `false`.

### Zed Task Picker

`examples/tasks.json` gains these terminal-backed tasks:

- `Ferry: sync current file`
- `Ferry: sync current folder`
- `Ferry: choose path to sync...`

The first two pass Zed's active file or directory as the single path. The
interactive task starts from the active file's directory so Ferry's existing
nearest-ancestor lookup finds the intended `.ferry.toml`, including for a
Ferry project nested inside a larger Zed worktree. If there is no active file,
the user can run `ferry sync --select` from a terminal whose working directory
is inside the intended Ferry project.

Documentation also shows how projects can define stable named tasks such as
`Ferry: sync areas` with `ferry sync areas` for frequently used roots.

### Interactive path browser

`ferry sync --select` opens a terminal picker rooted at the configured local
and remote roots. It obtains only the direct children of the currently viewed
directory, merges local and remote entries by normalized relative path, sorts
them deterministically, and labels each entry `local`, `remote`, or `both`.
Directories and files are visually distinct.

Choosing a directory entry browses into it. A `Sync this folder` entry selects
the currently displayed directory, while choosing a file selects that file.
A parent entry navigates upward but is absent at the configured root. Escape
or the displayed cancel choice exits successfully without synchronization or
state changes.

The picker performs no recursive inventory and no writes while browsing.
This keeps startup fast for large remote trees. After one path is selected,
the picker closes and passes the normalized relative path to the same scoped
sync engine used by direct CLI paths and Zed Code Actions.

Paths matched by Ferry's current ignore rules are omitted. Display text is
sanitized so remote filenames cannot inject terminal control sequences.
Authentication, listing, and transport failures remain visible in the task
terminal and use Ferry's normal error classification.

## CLI Contract

The `sync` subcommand accepts one optional positional path and an interactive
selection flag:

```text
ferry sync
ferry sync PATH
ferry sync --select
ferry sync PATH --force
ferry sync --select --force
```

- No path and no `--select` retains the existing project-wide behavior.
- `PATH` selects exactly one file or directory.
- `--select` and `PATH` are mutually exclusive.
- A directory scope is recursive.
- `--force` retains the existing sync meaning: local wins for `BothChanged`
  and `Untracked` files. It remains available to deliberate CLI users but is
  not placed in the supplied Zed tasks or Code Actions.
- Global `--dry-run` works with direct and interactively selected scopes. The
  browser itself is always read-only; after selection, dry-run prints the
  planned work without changing local files, remote files/directories, or
  Ferry state.
- `--select` requires an interactive terminal. Without one it returns a clear
  error instructing the user to pass `PATH` directly.

Paths may be absolute local paths or paths relative to the configured
`local_root`, matching existing path-aware Ferry commands. Resolution is
lexical for remote-only paths, rejects `..` traversal and absolute paths that
escape `local_root`, and applies the existing safe canonicalization checks to
local entries. Unsafe symlink traversal is rejected.

An explicit path that resolves exactly to `local_root`--for example `.`, the
absolute local-root path, the picker root's `Sync this folder`, or Current
Folder for a file directly beneath the root--is represented as an explicit
root-directory scope. It is not passed through the existing helpers that
reject an empty relative path. This scope is distinct from omitting `PATH`:
it uses the new strict scoped inventory and directory-materialization rules,
whereas no `PATH` retains legacy project-wide behavior.

A direct `PATH` that exists neither locally nor remotely and has no matching
state record returns a clear `path not found locally or remotely` error. It is
never guessed to be a new empty directory. A stale state-only selection keeps
the existing non-deletion behavior and reports that neither side has an
entry.

## Scoped Sync Semantics

### Inventory

Project-wide sync continues to use the current whole-tree behavior. Scoped
sync builds an inventory limited to:

- the exact selected file; or
- files and directories equal to or beneath the selected directory.

The inventory is the union of matching local entries, remote entries, and
file records already present in Ferry state. Unrelated siblings and state
records are never classified, transferred, created, or removed.

The existing ignore configuration applies before entries reach the plan. The
selected Ferry metadata directory and temporary transfer names remain
protected by the existing matcher rules. `sync.include` is currently parsed
but unused by Ferry; scoped sync does not silently invent semantics for it.

Scoped inventory must be complete before any transfer or directory creation
begins. Unlike the legacy whole-project walker, a failure to list any remote
directory inside an explicit scope is fatal for that invocation. Ferry must
not treat entries beneath a failed listing as remotely absent, classify them
as `LocalOnly`, or upload them. Direct-file discovery likewise requires a
conclusive remote existence/type result before absence-based classification.

### Files

Each selected file uses Ferry's existing three-hash classification and action
matrix:

| State | Normal action |
| --- | --- |
| InSync | No operation |
| LocalChanged | Upload |
| RemoteChanged | Download |
| LocalOnly | Upload |
| RemoteOnly | Download |
| BothChanged | Report conflict |
| Untracked | Report conflict |

With explicit CLI `--force`, `BothChanged` and `Untracked` take the existing
local-wins upload path. Atomic local installation, atomic remote upload, hash
caching, state updates, and exit-code classification reuse the current Ferry
implementation.

As in current project-wide sync, clean entries may complete even when another
entry in the selected directory conflicts. Progress is saved, all conflicts
are reported, and the command returns conflict exit code `2` at the end. This
feature does not change sync into an all-or-nothing transaction.

### Directories

Selecting a directory ensures that directory and all discovered descendant
directories exist on both sides, subject to ignore rules:

- A remote-only directory is created locally before its downloaded children.
- A local-only directory is created remotely before its uploaded children.
- Empty selected directories and empty descendants are materialized on the
  missing side.
- Existing directory entries require no state record; Ferry state remains
  file-oriented.
- Missing entries are never interpreted as deletions. Ferry does not remove a
  directory or file merely because it exists on only one side.

A path that is a file on one side and a directory on the other is an explicit
type conflict. Ferry reports it and does not modify either entry. A path that
disappears or changes type between selection and execution is revalidated and
fails safely.

## Internal Architecture

The sync implementation is separated into scope resolution, inventory,
planning, and execution:

1. `SyncScope` has distinct `LegacyProject`, `RootDirectory`, and normalized
   `Path` variants. `RootDirectory` is the valid result for an explicit path
   equal to `local_root`; it is never encoded as an invalid empty ordinary
   path.
2. A scoped inventory collector walks only the required local and remote
   subtrees and collects both file and directory entries. Scoped collection
   is strict: an incomplete local or remote traversal returns an error before
   execution.
3. The file planner reuses the existing hash/state classification. Directory
   preparation produces only missing-directory creation operations.
4. The executor preserves the current sequential behavior, transfer helpers,
   dry-run output, conflict aggregation, and final state save.

The refactor must keep a no-scope call behaviorally compatible with the
existing project-wide command. Scoped logic is shared by direct CLI paths,
the selector result, and LSP actions rather than spawning and parsing another
`ferry` process from the language server.

The shared scoped-sync API is structured and non-printing. It returns typed
events, completed changes, conflicts, and cancellation/error information; it
never calls `println!` or `eprintln!`. Only the CLI adapter renders those
events to stdout/stderr. The LSP adapter maps them to Info or Warning
notifications. This separation is mandatory because any command output on
`ferry-lsp` stdout would corrupt its JSON-RPC stream.

The browser is isolated behind testable terminal-input, terminal-output,
local-listing, and remote-listing boundaries. Navigation tests do not require
a real terminal or FTP server. Directory listing is lazy and cached only for
the lifetime of one picker session; synchronization revalidates the selected
path instead of trusting the browsing cache.

## LSP Behavior

The two new commands use the existing asynchronous action coordinator so FTP
and filesystem work stays off the LSP protocol thread. Results are reported
through the same Info and Warning notifications as the existing manual
actions.

Both sync actions are save-first operations:

- Current-file sync refuses when that document has unsaved changes.
- Current-folder sync refuses when any document known to `ferry-lsp` beneath
  the selected directory has unsaved changes and asks the user to save all
  affected files before retrying.

A current-folder action also registers a scope generation before its worker
starts. Any `didOpen`, `didChange`, `didSave`, or `didClose` for a tracked
document equal to or beneath that directory invalidates the generation, even
if the event leaves the document clean. Shutdown invalidates it as well. The
worker checks the generation immediately before every local or remote
directory creation, upload, and local-file installation. Once invalidated it
performs no further transfer mutations and reports that the folder changed in
Zed and must be retried.

Mutating LSP operations for one Ferry project are serialized behind the
existing action coordinator while a folder action is active. Transfers that
finished before invalidation remain valid partial progress, matching normal
sync semantics; their state records are persisted before the operation lease
is released. A queued automatic or manual operation must re-resolve project
configuration and document state after it obtains the lease. This prevents a
mid-flight save from racing a stale directory upload or overwriting newer
state.

Zed does not expose dirty buffers from languages to which Ferry is not
attached. The task documentation therefore tells users to save all files
before running terminal-backed directory or picker tasks. Successful
downloads rely on Zed's existing external-file observation, as current Pull
does.

Shutdown, response correlation, and stale-document guards reuse the existing
manual-action infrastructure. A shutdown or stale current-document request
must not turn into an unguarded current-file write.

## Error Reporting and Safety

- Configuration and authentication failures retain exit code `3`.
- File conflicts retain exit code `2`.
- Type conflicts, unsafe paths, non-interactive picker use, and transport
  failures return a normal nonzero error with the selected relative path in
  the message.
- An incomplete scoped remote listing fails before any absence-based plan or
  mutation is allowed.
- The picker never prints credentials or connection secrets.
- Cancelling the picker performs no local, remote, or state writes.
- No supplied Zed action/task uses `--force`, deletion, or automatic sync.
- Manual actions remain scoped to the nearest resolved Ferry project.
- The existing `[editor]` defaults remain `false`; no configuration migration
  is required.

## Documentation

Update the root README and extension README with:

- the right-click limitation and supported Task Picker workflow;
- the three new task entries and two new Code Actions;
- direct path, directory, selector, force, and dry-run CLI examples;
- the single-selection and recursive-directory semantics;
- the save-all warning for terminal-backed directory sync;
- remote-only and empty-directory behavior;
- named per-project task examples; and
- the fact that sync never propagates deletion.

`examples/tasks.json` remains the copyable source for project task setup.

## Verification

### Unit and command tests

- Clap parsing for no path, one path, `--select`, mutual exclusion, force, and
  dry-run combinations.
- Absolute and relative path normalization, traversal rejection, symlink
  escape rejection, and local-root boundary checks.
- Exact-file and directory-prefix inventory filtering, including state-only
  records and exclusion of similarly prefixed siblings.
- Distinct legacy-project and explicit-root-directory scopes from direct
  `.`, absolute local-root, picker-root, and LSP current-folder inputs.
- Merged picker ordering and presence labels.
- Picker navigation into local-only, remote-only, and shared directories;
  parent navigation; choosing the current folder; choosing one file; and
  cancellation without writes.
- Terminal-control sanitization and non-interactive failure text.
- A direct path missing locally, remotely, and from state returns the defined
  not-found error without creating anything.
- LSP action titles/order, current file/folder scope derivation, dirty-buffer
  subtree checks, response correlation, background execution, and proof that
  the shared engine writes no stdout/stderr from an LSP worker.
- Mid-flight descendant open/change/save/close and shutdown events invalidate
  a folder action before its next transfer; completed progress is recorded
  and queued operations revalidate after serialization.

### FTP integration tests

- A remote-only selected directory downloads recursively and creates the
  local root and empty descendants.
- A local-only selected directory uploads recursively and creates remote empty
  descendants.
- An exact selected file changes without touching siblings.
- A selected directory excludes sibling directories with similar names.
- Local-changed and remote-changed files reconcile in one selected subtree.
- Conflicts return exit code `2` while already completed clean entries and
  state progress are preserved.
- Type conflicts do not overwrite either side.
- A nested remote-listing failure aborts scoped sync before local-only
  classification, upload, directory creation, or state mutation.
- Dry-run changes neither side, directory structure, nor state.
- Bare project-wide sync retains its established behavior.

### Final verification

- `cargo fmt --all -- --check`
- strict Clippy across all targets
- all Rust targets and documentation tests
- Zed extension tests and `wasm32-wasip2` check
- Docker-backed FTP integration tests
- a controlled Zed smoke test covering both new Code Actions, remote-only
  root selection, recursive folder sync, conflict reporting, and picker
  cancellation

