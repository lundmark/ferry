# Zed Force Pull and Remote Diff Design

**Date:** 2026-08-07
**Status:** Approved in conversation; pending written-spec review

## Objective

Extend Ferry's Zed Code Actions with two manual, project-scoped operations:

- `Ferry: Compare with Remote`, which opens Zed's native diff view without
  changing the local file or Ferry state; and
- `Ferry: Force Pull (overwrite local)`, which replaces the saved local file
  with the remote version only after a second confirmation in Zed.

Both operations must refuse to run while the active document has unsaved
editor changes. The existing normal Pull, Push, and Compile-check actions
remain available.

## Scope

This work covers:

- two new Ferry LSP commands and Code Actions;
- read-only retrieval of one remote file;
- native Zed diff launch through `zed --diff`;
- private temporary snapshot creation and cleanup;
- LSP dirty-document tracking;
- a native LSP confirmation request for Force Pull;
- a guarded, atomic installation path for confirmed remote content;
- changing automatic editor-sync defaults so both `pull_on_open` and
  `push_on_save` default to `false` while remaining configurable per project;
- documentation and automated tests for the new behavior.

This work does not add an Accept button inside Zed's diff UI, automatically
apply content from the diff, add a general-purpose diff command to the Ferry
CLI, or modify Zed itself. The existing CLI `pull --force` behavior remains
unchanged and does not gain an interactive confirmation.

## Existing Behavior and Constraints

`ferry-lsp` currently advertises three commands:

- `ferry.pull`
- `ferry.push`
- `ferry.compile`

Manual commands are resolved again at execution time and run on a detached
worker so network operations do not block the LSP protocol loop. Normal Pull
already passes `force = false`; the core `pull_one` operation already supports
`force = true`, atomically replaces the local file, and updates Ferry state.

The LSP currently advertises `TextDocumentSyncKind::NONE`, so it cannot tell
whether the Zed buffer has changes that are not on disk. Its protocol loop also
ignores client responses, so it cannot yet correlate a response to an LSP
`window/showMessageRequest` confirmation.

The installed Zed CLI supports:

~~~text
zed --diff <OLD_PATH> <NEW_PATH>
~~~

Launching this command opens Zed's native file-diff view. Zed's extension API
does not expose an API for adding Ferry-specific controls inside that view, so
review and overwrite remain separate actions.

## User Experience

For a local file that resolves through the nearest `.ferry.toml`, `Ctrl+.` and
the editor Code Actions menu list these actions in this order:

1. `Ferry: Pull`
2. `Ferry: Compare with Remote`
3. `Ferry: Force Pull (overwrite local)`
4. `Ferry: Push`
5. `Ferry: Compile-check`

The two `[editor]` booleans affect only automatic open/save behavior. They do
not hide or disable manual Code Actions.

### Compare with Remote

When the user selects Compare with Remote, Ferry:

1. resolves the active file through the nearest Ferry project;
2. refuses the command if that document is dirty in Zed and records the clean
   document revision plus the saved local-file identity;
3. retrieves the current remote bytes without modifying the local file or
   persisted Ferry state;
4. writes a unique, read-only snapshot in Ferry's private process-temporary
   directory;
5. immediately before launch, rechecks that the document is still clean at the
   recorded revision and that the saved local identity is unchanged;
6. launches `zed --diff <local-path> <snapshot-path>` only if both guards still
   match; and
7. reports an error in Zed if retrieval, snapshot creation, or launch fails.

The saved local version is the old/left side of the diff. The incoming remote
version is the new/right side. Compare never pulls, updates a state hash, or
changes automatic-sync settings.

### Force Pull

When the user selects Force Pull, Ferry:

1. resolves the active file and refuses if the document is dirty;
2. retrieves the remote bytes and records the identity of the saved local
   file without changing either side;
3. sends Zed a warning-level `window/showMessageRequest` naming the relative
   file and explaining that the saved local version will be overwritten;
4. offers `Overwrite local file` and `Cancel` actions;
5. treats `Cancel`, closing the prompt, a null response, an invalid response,
   or LSP shutdown as cancellation;
6. after an affirmative response, rechecks that the Zed document is clean and
   that the saved local file still has the identity captured before the
   confirmation; and
7. atomically installs the already-retrieved remote bytes and updates Ferry
   state.

If the document became dirty or the saved local file changed while the prompt
was open, Ferry aborts and asks the user to save and retry. It never silently
overwrites a version different from the one for which confirmation was shown.

The `workspace/executeCommand` request is acknowledged once the command is
accepted for processing, matching the existing LSP behavior. Completion,
cancellation, and failure feedback are delivered through Zed notifications.
Cancellation may be silent; all actual failures produce a warning.

### Normal Pull and Dirty Documents

Normal manual Pull also refuses to run on a dirty document. It uses a two-phase
read/commit path: remote retrieval and classification prepare a result without
writing, then Ferry rechecks the clean document revision and saved local
identity immediately before any atomic replacement. A `didChange` received
while retrieval is pending invalidates the commit and produces a save-and-retry
warning.

An automatic pull-on-open is skipped if Zed opens a restored buffer whose
contents already differ from the saved disk file, and uses the same final guard
if the user edits while its remote work is pending. These guards prevent Ferry
from changing the disk underneath an unsaved editor buffer.

Push and Compile-check retain their existing behavior. Save notifications mark
a document clean before any configured push-on-save work is queued.

## Project Configuration

Automatic operations remain controlled by the nearest project's
`.ferry.toml`:

~~~toml
[editor]
pull_on_open = false
push_on_save = false
~~~

Both values default to `false` when the section or either key is absent. This
is an intentional compatibility change: `pull_on_open` currently defaults to
`true`. Projects that want automatic pulls must opt in explicitly.

No additional setting is required for Compare or Force Pull. Manual actions
are available whenever the active file resolves into a valid Ferry project.

## Component Design

### 1. Read-only remote retrieval

Add a single-file core operation that accepts a configuration path and a safe
relative path and returns a value containing at least:

- the remote bytes;
- the SHA-256 identity of those bytes; and
- the normalized relative path needed for messages and state updates.

The operation uses the existing configuration, path validation, remote-root
joining, FTP connection, and safe error categorization. A missing remote file
is an error for both Compare and Force Pull.

Retrieval must not write the local file, save Ferry state, or mutate any
project metadata. It may use in-memory data while calculating the remote
identity, but that data is discarded unless a confirmed Force Pull later
installs the snapshot.

### 2. Guarded remote installation

Add a focused core operation that installs retrieved remote bytes only when
the current saved local identity still matches the expected identity captured
before confirmation. It reuses Ferry's existing atomic sibling-temporary-file
plus rename behavior and state-entry update logic.

The identity represents both presence and content, so deletion, creation, or
replacement of the local file during confirmation is detected. A mismatch is
a conflict and causes no local or state mutation.

The guarded installer owns the final disk check and write so the invariant is
enforced even if a non-LSP caller is added later. The existing `pull_one`
operation and CLI semantics remain compatible; shared download/state-update
logic may be factored out only as needed to prevent duplication.

### 3. Document dirty-state tracker

Change LSP synchronization from `NONE` to incremental change notifications and
handle `didOpen`, `didChange`, `didSave`, and `didClose`:

- On `didOpen`, compare the supplied buffer text with the saved disk bytes.
  Mark the document dirty if they differ.
- On any valid `didChange`, advance its document revision, mark the document
  dirty, and invalidate pending Pull, Compare, or Force Pull work for that
  document. Ferry does not need to reconstruct or retain the full editor
  buffer.
- On `didSave`, mark it clean and preserve the existing optional push-on-save
  behavior.
- On `didClose`, remove its tracking entry.

The tracker is updated synchronously in the protocol loop before relevant work
is queued. Each guarded operation records the current document revision and
uses a one-shot operation token. A processed `didChange` cancels the token;
the final side-effect step must atomically claim an uncancelled token for the
same clean revision immediately before a disk replacement or diff launch. If
the edit wins that race, the operation aborts. If the final claim wins, a later
edit is treated as occurring after the clean-state commit point. Malformed
notifications do not clear a dirty flag.

Remote preparation results return to the protocol coordinator before their
side effect. Compare is revalidated before the coordinator launches Zed.
Normal Pull and Force Pull are revalidated before the coordinator queues their
guarded installer, and the installer claims the token at its final mutation
boundary. The guarded local-identity check remains the disk-level backstop.

Code Actions remain visible for dirty files so the commands stay discoverable;
execution produces a concise `save the file and retry` warning.

### 4. Confirmation coordinator

Extend the protocol loop with a monotonic namespace for server-originated
request IDs and a map from pending confirmation IDs to prepared Force Pull
data. Server-originated IDs must not collide with one another. Responses that
do not match a pending Ferry request are ignored as they are today.

The Force Pull worker retrieves remote data and returns a confirmation proposal
to the protocol loop rather than waiting for a client response itself. The
protocol loop sends `window/showMessageRequest` and continues serving LSP
traffic. This preserves responsiveness and lets it process the corresponding
client response, edits, shutdown, and other requests while the prompt is open.

An affirmative response queues guarded installation on the existing worker
only after the protocol loop rechecks dirty state. Other responses drop the
prepared data. Pending confirmations are dropped on shutdown. A second Force
Pull for the same document supersedes and cancels the earlier pending request;
commands for other files may remain independently pending.

### 5. Snapshot manager

The LSP process creates one private temporary root using the platform temporary
directory. Every Compare creates a unique file under that root. Snapshot names
retain the source file's extension for Zed syntax detection but do not reuse
untrusted relative paths as directories.

Creation must use exclusive files or a trusted temporary-file facility so
symlinks and path traversal cannot redirect writes. Completed snapshots are
made read-only where the platform supports permissions. They remain available
for the lifetime of `ferry-lsp`, because Zed may read them after the launch
process exits.

Normal LSP shutdown removes the process root. Ferry also removes only its own
recognizably named stale snapshot directories during a later startup; it never
recursively removes a broad temporary directory or follows symlinks.

### 6. Zed diff launcher

Introduce a small injectable launcher boundary. Production launches:

~~~text
zed --diff <absolute-local-path> <absolute-snapshot-path>
~~~

using the `zed` executable visible in `ferry-lsp`'s environment. In the Zed
Flatpak this resolves to the bundled CLI. The process is spawned without
waiting for the Zed window to close. Failure to find or spawn the executable
produces a path-specific warning and keeps the snapshot until normal cleanup.

Tests use a fake launcher and assert the exact argument ordering without
opening a GUI. No new `.ferry.toml` option is added for the Zed executable.

### 7. LSP operations boundary

Extend the existing injectable `FileOperations` boundary, or split it into
equally focused interfaces if that keeps tests smaller, to cover:

- remote retrieval;
- guarded installation;
- snapshot creation; and
- diff launch.

Network and filesystem-heavy operations stay on the existing worker. The
protocol loop remains responsible for request/response correlation, document
state, and scheduling. Neither layer logs configuration contents or sensitive
authentication details.

## Data Flows

### Compare flow

~~~text
Code Action
  -> validate URI and Ferry project
  -> reject dirty document and record clean revision/local identity
  -> worker retrieves remote bytes without mutation
  -> worker creates unique snapshot
  -> protocol coordinator rechecks clean revision/local identity
  -> coordinator claims operation token
  -> coordinator spawns `zed --diff local snapshot`
  -> Zed displays native diff
~~~

### Normal Pull flow

~~~text
Code Action or enabled open event
  -> reject dirty document and record clean revision/local identity
  -> worker prepares normal pull without mutation
  -> protocol coordinator rechecks clean revision/local identity
  -> worker claims operation token at the commit boundary
  -> guarded atomic replacement and Ferry state update when remote wins
~~~

### Force Pull flow

~~~text
Code Action
  -> validate URI and Ferry project
  -> reject dirty document
  -> worker retrieves remote bytes and captures local identity
  -> protocol loop sends Zed confirmation request
  -> user confirms
  -> protocol loop rechecks clean document revision/local identity
  -> worker claims operation token and verifies saved local identity
  -> atomic local replacement and Ferry state update
  -> Zed file watcher observes saved-file change
~~~

## Errors and Feedback

User-visible warnings include the relative path and a safe summary. They never
include passwords or raw configuration contents.

- Dirty buffer: `save the file and retry`; no network or local mutation when
  detected before work begins. If an edit arrives during remote work, Ferry
  discards prepared data, removes any unlaunched comparison snapshot, and does
  not launch a diff, replace the local file, or update state.
- Missing remote: report that the remote file does not exist; no snapshot,
  local write, or state update.
- Authentication/transport/configuration failure: retain Ferry's safe error
  categories and recommend running a Ferry task for details.
- Local identity changed before install: report a conflict and require retry;
  no local write or state update.
- Snapshot failure: report comparison preparation failure; local and state
  remain unchanged.
- Zed launch failure: report that the native diff could not be opened; local
  and state remain unchanged.
- Confirmation cancellation or dismissal: no local write or state update.
- Worker unavailable or LSP shutdown: fail safely and drop prepared data.

A successful Compare reports that the native diff was opened. A successful
Force Pull uses the existing transfer feedback style and reports the file as
transferred.

## Concurrency and Safety Invariants

- The protocol loop never blocks on FTP, filesystem installation, a GUI
  process, or user confirmation.
- Compare never persists Ferry state or changes the project tree.
- A processed `didChange` invalidates pending Pull, Compare, and Force Pull
  work unless its final side-effect token was already claimed for the same
  clean revision.
- Compare revalidates document revision and saved local identity immediately
  before opening Zed's diff.
- Normal Pull revalidates document revision and saved local identity at its
  guarded commit boundary.
- Force Pull writes only bytes that were retrieved before the confirmation and
  only over the exact saved local identity that was confirmed.
- An unsaved document is never knowingly overwritten on disk.
- All local project paths and remote relative paths pass existing Ferry path
  validation.
- Temporary snapshots cannot escape Ferry's private temporary root.
- No operation exposes credentials in notifications, command arguments, test
  output, or documentation.

## Testing Strategy

### Unit tests

Core tests cover:

- read-only retrieval returns exact remote bytes and hash;
- retrieval rejects escaping relative paths before connecting;
- missing remote and transport errors are categorized and path-specific;
- retrieval does not create or change the local file or persisted state;
- guarded installation succeeds for a matching local identity;
- guarded installation rejects changed, deleted, or newly created local files
  without altering state;
- guarded installation remains atomic.

Snapshot and launcher tests cover:

- unique snapshot creation with a preserved extension;
- containment under the private temporary root;
- traversal-like and unusual filenames cannot escape the root;
- snapshot content matches remote bytes;
- read-only permissions where supported;
- cleanup targets only Ferry-owned snapshot roots;
- exact `zed --diff local snapshot` argument order; and
- launch failures return safe errors.

LSP tests cover:

- all five commands are advertised and all five Code Actions appear in order;
- manual actions remain independent of automatic editor settings;
- incremental sync capabilities and open/change/save/close dirty transitions;
- a restored dirty buffer is detected on open;
- normal Pull, Compare, and Force Pull reject dirty documents;
- edits received while normal Pull or Compare remote work is pending invalidate
  the prepared operation, causing no disk write, state update, or diff launch;
- automatic pull-on-open is skipped for a restored dirty buffer;
- Force Pull sends a warning-level `window/showMessageRequest` with the two
  approved action labels;
- affirmative, cancelled, dismissed, malformed, unknown, and shutdown response
  handling;
- the protocol loop remains responsive while remote work and confirmation are
  pending;
- a dirty document or changed disk identity after prompting aborts install;
- a second same-file confirmation supersedes the first;
- retrieval, snapshot, launcher, and install failures produce safe feedback;
  and
- no notification contains secret configuration values.

Configuration tests change the absent-field expectation to
`pull_on_open = false` and retain `push_on_save = false`, while verifying that
explicit true/false values still work independently.

### Integration and regression tests

Controlled FTP integration tests prove:

- Compare retrieves remote content while leaving the local file and persisted
  state byte-for-byte unchanged;
- confirmed Force Pull overwrites a locally changed file with the retrieved
  remote content and updates state;
- cancellation performs no mutation;
- a local change between preparation and installation is rejected; and
- existing normal Pull, Push, Compile-check, automatic-event, and CLI force
  behavior continue to pass.

GUI behavior is not exercised by automated tests. A fake launcher replaces
Zed in the suite, and live FTP credentials are never used.

## Documentation and Manual Verification

Update the README and Zed integration documentation to describe:

- both automatic settings defaulting to `false`;
- all five Code Actions;
- the save-first requirement;
- local-left/remote-right diff orientation;
- Force Pull confirmation and overwrite semantics; and
- native diff launch requiring the `zed` CLI visible to `ferry-lsp`.

Final manual verification uses a disposable Ferry project and controlled
remote fixture, not the live 3S remote tree:

1. Open a clean local file with a differing remote version.
2. Confirm `Ctrl+.` lists all five actions.
3. Run Compare and confirm Zed displays local on the left and remote on the
   right without changing the local file.
4. Make an unsaved edit and confirm Pull, Compare, and Force Pull refuse.
5. Save or discard the edit, run Force Pull, cancel, and confirm no change.
6. Run Force Pull again, confirm overwrite, and verify Zed observes the remote
   content on disk.
7. Confirm projects without explicit editor booleans perform neither automatic
   pull-on-open nor push-on-save.

## Acceptance Criteria

- A Ferry file offers Pull, Compare with Remote, Force Pull, Push, and
  Compile-check through Zed Code Actions.
- Compare opens Zed's native diff with saved local content on the left and
  current remote content on the right.
- Compare never modifies the local file or Ferry state.
- Pull, Compare, and Force Pull refuse dirty Zed documents, including edits
  received while their asynchronous preparation is pending.
- Force Pull requires explicit Zed confirmation and overwrites only the saved
  local identity that was confirmed.
- Cancel, dismissal, remote failure, local races, snapshot failure, and launch
  failure preserve the local file and Ferry state.
- Confirmed Force Pull atomically installs the retrieved remote bytes and
  records the new state.
- Automatic pull-on-open and push-on-save are independently configurable per
  project and both default to `false`.
- Existing Ferry commands and CLI force-pull behavior remain compatible.
- Automated tests use controlled fixtures and never expose or contact live 3S
  credentials.
