# Zed Scoped File and Directory Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** Add conflict-safe, single-path Ferry synchronization for files and recursive directories, a terminal path picker that can discover remote-only roots, and Zed Code Actions/tasks for the current file and folder.

**Architecture:** Keep the no-argument sync command behaviorally compatible, while routing explicit paths through a strict scoped inventory that tracks files and directories on both sides. The shared scoped engine returns typed events and accepts an atomic commit gate; the CLI renders events, while ferry-lsp supplies a scope guard that editor lifecycle notifications can invalidate before each destination commit. A lazy, line-oriented terminal picker resolves exactly one path and then calls the same engine.

**Tech Stack:** Rust 2024, clap 4, anyhow, suppaftp 6, lsp-server/lsp-types, serde, tempfile, testcontainers, Zed tasks and development extensions.

---

## Source documents and guardrails

- Approved design: docs/superpowers/specs/2026-08-10-zed-scoped-sync-design.md
- Working tree: /home/simon/code/zed_ftp/.worktrees/zed-code-actions-keybinding
- Branch: fix/zed-code-actions-keybinding-config
- Existing pull/compare/force-pull behavior and its remote-consistency guarantees must remain unchanged.
- Do not read, print, copy, or modify credentials in /home/simon/code/3s/.ferry.toml.
- Do not run any new sync action against the live 3S FTP tree. Use unit fakes and the disposable Docker FTP fixture.
- Preserve unrelated files in the durable repository, especially the untracked extensions/ferry/extension.wasm outside this worktree.
- Use @superpowers:test-driven-development for every behavior change.
- Before claiming completion, use @superpowers:verification-before-completion and @superpowers:requesting-code-review.

## File and responsibility map

### New files

- src/commands/sync/scope.rs
  - Root-aware SyncScope parsing and containment.
  - Distinguishes legacy no-argument project sync, explicit configured-root sync, and one normalized path.
- src/commands/sync/inventory.rs
  - Complete local/remote file-and-directory inventory for explicit scopes.
  - Strict remote traversal, ignore filtering, type detection, and subtree boundaries.
- src/commands/sync/commit.rs
  - Object-safe CommitGate contract, CommitDecision, and unconditional CLI implementation.
- src/commands/sync/picker.rs
  - Lazy merged local/remote directory browser.
  - Testable input/output boundary, presence labels, navigation, cancellation, and display sanitization.
- tests/scoped_sync_integration.rs
  - Docker-backed exact-file, recursive-directory, empty-directory, conflict, root, and sibling-isolation tests.

### Modified files

- src/main.rs
  - One optional sync path, --select parsing, mutual exclusion, and CLI dispatch.
- src/project.rs
  - Root-aware relative path result without weakening existing file resolution.
- src/ftp.rs
  - Strict raw LIST parser and strict listing trait used only by explicit scopes and the picker.
- src/commands/mod.rs
  - Any shared transfer/commit exports required by sync.
- src/commands/sync.rs
  - Structured outcomes, CLI renderer, legacy adapter, scoped orchestration, state persistence, and error mapping.
- src/commands/push.rs
  - Crate-private staged remote upload and guarded final rename/state update.
- src/commands/pull.rs
  - Crate-private staged local write and guarded final rename/state update.
- src/lsp/document_state.rs
  - Exact-file/directory scope registrations, dirty-descendant validation, reusable atomic commit claims, and lifecycle invalidation.
- src/lsp.rs
  - Two commands/actions, scope derivation, worker execution, typed feedback, serialization, and protocol tests.
- tests/cli_test.rs
  - CLI compatibility, argument errors, non-interactive selector behavior, and exit codes.
- tests/dry_run_integration.rs
  - Scoped file/directory and empty-directory non-mutation.
- tests/editor_sync_integration.rs
  - Real FTP structured sync and LSP stdout framing coverage.
- examples/tasks.json
  - Current-file sync, current-folder sync, and choose-path tasks.
- README.md
  - CLI, Task Picker, remote-only directory, deletion, safety, and right-click limitations.
- extensions/ferry/README.md
  - Seven Code Actions and task workflow.
- extensions/ferry/extension.toml
  - Description updated to mention scoped sync.

No new runtime dependency is required. The picker uses std::io::IsTerminal and a numbered line-oriented menu so it remains testable and works in Zed's terminal.

### Task 1: Add the one-path CLI contract and root-aware scope model

**Files:**
- Create: src/commands/sync/scope.rs
- Modify: src/commands/sync.rs
- Modify: src/main.rs
- Modify: src/project.rs
- Test: src/main.rs
- Test: src/project.rs
- Test: src/commands/sync/scope.rs

- [ ] **Step 1: Write failing clap tests for the sync grammar**

Add tests in src/main.rs using Cli::try_parse_from. Cover these exact cases:

~~~rust
#[test]
fn sync_accepts_zero_or_one_path_or_select() {
    assert!(matches!(
        Cli::try_parse_from(["ferry", "sync"]).unwrap().cmd,
        Cmd::Sync { path: None, select: false, force: false }
    ));
    assert!(matches!(
        Cli::try_parse_from(["ferry", "sync", "areas"]).unwrap().cmd,
        Cmd::Sync { path: Some(path), select: false, force: false }
            if path == "areas"
    ));
    assert!(matches!(
        Cli::try_parse_from(["ferry", "sync", "--select"]).unwrap().cmd,
        Cmd::Sync { path: None, select: true, force: false }
    ));
}

#[test]
fn sync_rejects_multiple_paths_and_path_plus_select() {
    assert!(Cli::try_parse_from(["ferry", "sync", "one", "two"]).is_err());
    assert!(Cli::try_parse_from(["ferry", "sync", "one", "--select"]).is_err());
}
~~~

Also assert that --force remains accepted with either PATH or --select.

- [ ] **Step 2: Run the clap tests and verify RED**

Run:

    cargo test --bin ferry sync_accepts_zero_or_one_path_or_select
    cargo test --bin ferry sync_rejects_multiple_paths_and_path_plus_select

Expected: FAIL because Cmd::Sync has no path/select fields.

- [ ] **Step 3: Write failing root-containment tests**

In src/project.rs, specify a root-aware result:

~~~rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelativeToRoot {
    Root,
    Path(String),
}
~~~

Test:

- an absolute path equal to local_root returns Root;
- "." resolved from local_root returns Root through the sync scope layer;
- an absolute descendant returns Path("nested/file.c");
- a new remote-only lexical descendant is accepted when its nearest existing ancestor is contained;
- an absolute path and a dangling symlink escaping local_root are rejected;
- existing relative_to_local_root still rejects the root so current file-only callers do not silently change behavior.

- [ ] **Step 4: Run the path tests and verify RED**

Run:

    cargo test project::tests::root_aware

Expected: FAIL because RelativeToRoot and the root-aware resolver do not exist.

- [ ] **Step 5: Implement the minimal scope types**

In src/commands/sync/scope.rs, use this public shape:

~~~rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncScope {
    LegacyProject,
    RootDirectory,
    Path(String),
}

pub fn from_cli_path(local_root: &Path, input: Option<&str>) -> Result<SyncScope>;
~~~

Rules:

- None becomes LegacyProject.
- "." and an absolute local_root become RootDirectory.
- one relative or absolute descendant becomes Path with safe forward-slash state-key form.
- empty strings, parent traversal, non-UTF-8 paths, and containment escapes error.
- do not infer whether Path is a file or directory here; inventory does that using both sides.

Add project::relative_to_local_root_or_root and implement existing relative_to_local_root by accepting only RelativeToRoot::Path.

- [ ] **Step 6: Update clap and preserve legacy dispatch**

Change Cmd::Sync to:

~~~rust
Sync {
    path: Option<String>,
    #[arg(long, conflicts_with = "path")]
    select: bool,
    #[arg(long)]
    force: bool,
}
~~~

Rename the current sync implementation entry point to a private legacy adapter if necessary. Add a compiling run_cli signature in src/commands/sync.rs:

~~~rust
pub fn run_cli(
    config_path: &Path,
    path: Option<&str>,
    select: bool,
    force: bool,
    mode: ExecutionMode,
) -> Result<()>;
~~~

At this checkpoint, no path/no selector must call the existing project-wide implementation unchanged. Explicit path and selector branches may return a precise temporary "scoped sync is not implemented yet" error until later tasks; they must compile and must never fall back to whole-project sync.

- [ ] **Step 7: Run focused and compatibility tests**

Run:

    cargo test --bin ferry
    cargo test project::tests
    cargo test commands::walk::walk_remote_tests
    cargo test --test sync_integration --no-run

Expected: PASS.

- [ ] **Step 8: Commit**

    git add src/main.rs src/project.rs src/commands/sync.rs src/commands/sync/scope.rs
    git commit -m "feat: add scoped sync CLI contract"

### Task 2: Add fail-closed FTP directory listings

**Files:**
- Modify: src/ftp.rs
- Test: src/ftp.rs

- [ ] **Step 1: Write strict raw-listing parser tests**

Extract a helper whose input is the remote directory plus raw LIST lines. Tests must prove:

~~~rust
#[test]
fn strict_listing_rejects_one_malformed_line_among_valid_entries() {
    let lines = vec![
        VALID_POSIX_FILE.to_string(),
        "\u{1b}[31mmalformed".to_string(),
        VALID_POSIX_DIRECTORY.to_string(),
    ];
    let error = parse_listing_strict("/root", &lines).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("ftp list /root"));
    assert!(!message.contains('\u{1b}'));
}

#[test]
fn strict_listing_accounts_for_blank_dot_and_dotdot_records() {
    // Blank records are ignored deliberately. Parsed . and .. entries remain
    // accounted records and are later skipped by traversal.
}
~~~

Also retain a test proving the tolerant list parser still drops malformed lines for legacy operations.

- [ ] **Step 2: Run and verify RED**

Run:

    cargo test ftp::tests::strict_listing

Expected: FAIL because parse_listing_strict does not exist.

- [ ] **Step 3: Implement strict listing without changing legacy list**

Add:

~~~rust
pub trait StrictRemote: Remote {
    fn list_dir_strict(&mut self, dir: &str) -> Result<Vec<Entry>>;
}

impl Ftp {
    pub fn list_strict(&mut self, dir: &str) -> Result<Vec<Entry>>;
}
~~~

Implementation requirements:

- Fetch raw lines once.
- Ignore only blank records explicitly.
- Every other raw line must parse with suppaftp::list::File::from_posix_line.
- Convert every parsed line into Entry, including "." and ".."; traversal accounts for those.
- On failure, report the sanitized directory and record index, not raw attacker-controlled text.
- Keep Ftp::list and Remote::list_dir behavior byte-for-byte compatible for legacy callers.
- StrictRemote for Ftp calls list_strict.
- Test fakes implement StrictRemote explicitly; do not provide a default that quietly delegates to tolerant list_dir.

- [ ] **Step 4: Run focused and existing FTP tests**

Run:

    cargo test ftp::tests
    cargo test commands::walk::walk_remote_tests

Expected: PASS, including the existing tolerant broken-subdirectory behavior.

- [ ] **Step 5: Commit**

    git add src/ftp.rs
    git commit -m "feat: add strict FTP directory listings"

### Task 3: Build a complete, non-mutating scoped inventory

**Files:**
- Create: src/commands/sync/inventory.rs
- Modify: src/commands/sync.rs
- Modify: src/commands/sync/scope.rs
- Test: src/commands/sync/inventory.rs

- [ ] **Step 1: Write failing inventory model tests**

Use these core types:

~~~rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InventoryEntry {
    pub local: Option<EntryKind>,
    pub remote: Option<EntryKind>,
    pub in_state: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedInventory {
    pub scope: SyncScope,
    pub entries: BTreeMap<String, InventoryEntry>,
}
~~~

Tests with a fake StrictRemote must cover:

- exact local file;
- exact remote-only file;
- local-only directory with nested and empty descendants;
- remote-only directory with nested and empty descendants;
- explicit RootDirectory;
- state-only records beneath a directory scope;
- state sibling "area-old/x.c" excluded when selecting "area";
- ignore rules applied to local and remote entries using local_root.join(relative);
- file-versus-directory type presence retained for later conflict reporting;
- a path on neither side and absent from state returns "path not found locally or remotely";
- a stale state-only exact path is retained and does not become a deletion;
- strict nested LIST failure aborts the entire inventory;
- an unsafe server-supplied child name aborts strict traversal instead of being skipped.
- a local-only nested file whose first remote-only ancestor is absent is still inventoried;
- a local-only nested directory whose inner remote ancestor is absent is still inventoried;
- failure while strictly listing any existing remote ancestor is fatal, never absence;
- a generic FTP 550/listing error is never reinterpreted as a missing parent;

- [ ] **Step 2: Run inventory tests and verify RED**

Run:

    cargo test commands::sync::inventory::tests

Expected: FAIL because the inventory module does not exist.

- [ ] **Step 3: Implement strict local collection**

Implement a local collector that records directories as well as files.

Requirements:

- Use symlink_metadata before following an entry.
- Reject a selected symlink or descendant symlink whose canonical target escapes local_root.
- Never descend into Ferry's state directory.
- Apply Matcher with the correct is_dir flag.
- Record empty directories.
- Preserve forward-slash relative keys.
- Fail on read_dir/metadata errors; an explicit scope may not use an incomplete local inventory.

- [ ] **Step 4: Implement strict remote collection**

Use StrictRemote::list_dir_strict.

Requirements:

- Resolve a direct Path by descending one segment at a time from the
  configured remote root. Strictly list only the root or a child already
  proven to be a directory; never start by listing an unproven immediate
  parent.
- Absence of the next trusted child from a successful, complete strict
  listing proves that the remaining remote subtree is absent. This is how
  local-only nested paths with nonexistent remote ancestors are represented.
- Any LIST error for a proven directory remains fatal. Never interpret a
  generic FTP 550, SIZE failure, or NLST failure as proof of absence.
- RootDirectory is known to be a directory.
- If a server cannot provide a complete, typed strict LIST for a traversed
  directory, fail closed instead of falling back to tolerant/exact probes.
- Recurse only beneath the selected directory.
- Record every directory before recursion so empty directories survive.
- Treat "." and ".." as accounted and skipped.
- A name outside the current directory/root is an error.
- Any nested listing error aborts before returning inventory.
- Never call the tolerant walk_remote from explicit scopes.

- [ ] **Step 5: Add state entries and validate selection**

After both sides are collected:

- mark state records equal to or beneath the selected path;
- preserve segment-boundary matching;
- return not-found only when local, remote, and matching state are all absent;
- do not classify or mutate anything in this module.

- [ ] **Step 6: Run focused tests and static safety checks**

Run:

    cargo test commands::sync::inventory::tests
    cargo test commands::sync::scope::tests
    rg -n "println!|eprintln!" src/commands/sync/inventory.rs src/commands/sync/scope.rs

Expected: tests PASS and rg returns no matches.

- [ ] **Step 7: Commit**

    git add src/commands/sync.rs src/commands/sync/scope.rs src/commands/sync/inventory.rs
    git commit -m "feat: collect strict scoped sync inventory"

### Task 4: Introduce structured outcomes and an atomic commit-gate contract

**Files:**
- Create: src/commands/sync/commit.rs
- Modify: src/commands/sync.rs
- Modify: src/commands/push.rs
- Modify: src/commands/pull.rs
- Modify: src/commands/file_transfer.rs
- Test: src/commands/sync/commit.rs
- Test: src/commands/push.rs
- Test: src/commands/pull.rs

- [ ] **Step 1: Write failing commit-gate contract tests**

Define the object-safe boundary:

~~~rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitDecision {
    Committed,
    Cancelled,
}

pub trait CommitGate: Send + Sync {
    fn is_current(&self) -> bool;

    // The implementation must invoke mutation at most once. A guarded
    // implementation atomically orders the mutation before or after invalidation.
    fn commit(
        &self,
        mutation: &mut dyn FnMut() -> Result<()>,
    ) -> Result<CommitDecision>;
}

pub struct UnconditionalCommitGate;
~~~

Tests prove the unconditional gate invokes the closure once and returns Committed, including a propagated closure error.

- [ ] **Step 2: Write failing local staging denial tests**

Expose StagedLocalWrite and stage_local_write as pub(crate). Add a guarded helper that:

- requires the target parent to be an already-existing, verified directory;
- stages bytes in a sibling temp without creating any directory;
- asks CommitGate only after staging;
- inside the claim revalidates the parent and destination, then renames the
  temp and updates the in-memory state record;
- on Cancelled drops the staged object and leaves destination, directories,
  and state unchanged.

Use a fake gate that flips from current to cancelled after staging. Assert
original destination bytes, original state, unchanged directories, and temp
cleanup. Add a case where the parent disappears before staging and assert the
operation fails without recreating it.

- [ ] **Step 3: Write failing remote staging denial tests**

Introduce a small crate-private RemoteWrite trait implemented by Ftp and by a deterministic fake:

~~~rust
pub(crate) enum RemoteDestinationSnapshot {
    Missing,
    File {
        size: u64,
        modified: DateTime<Utc>,
        sha256: String,
    },
    Directory,
}

pub(crate) trait RemoteWrite {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()>;
    fn rename(&mut self, from: &str, to: &str) -> Result<()>;
    fn rm(&mut self, path: &str) -> Result<()>;
    fn mkdir(&mut self, path: &str) -> Result<()>;
    fn mkdir_scoped_strict(&mut self, path: &str) -> Result<()>;
    fn mtime(&mut self, path: &str) -> Result<DateTime<Utc>>;
    fn destination_snapshot(&mut self, path: &str) -> Result<RemoteDestinationSnapshot>;
}
~~~

Create StagedRemoteWrite holding temp/target metadata. The denial test must pause after temp upload, invalidate the gate, then assert:

- destination bytes unchanged;
- rename never called;
- state record unchanged;
- temp removal attempted.

Capture temp mtime before the final claim so no metadata network round-trip occurs after destination rename but before the state record update.
StagedRemoteWrite requires an already-materialized remote parent and never
calls mkdir. Add a missing-parent case that fails without uploading or creating
anything. The Ftp destination_snapshot implementation uses the strict
root-to-parent discovery contract from Task 3 and hashes a present file.
`mkdir_scoped_strict` issues one exact MKD and propagates every error; it must
not call the existing tolerant `Ftp::mkdir` fallback or reinterpret a generic
550 as success. A deterministic fake test proves a 550/error cannot be accepted
through a tolerant listing.

- [ ] **Step 4: Run staging tests and verify RED**

Run:

    cargo test commands::sync::commit::tests
    cargo test commands::pull::staging_tests
    cargo test commands::push::staging_tests

Expected: FAIL because guarded staged APIs do not exist.

- [ ] **Step 5: Implement the minimal staged transfer APIs**

Add immutable planning snapshots:

~~~rust
pub(crate) struct ExpectedLocalDestination {
    // absent, or the inventoried file identity/hash/type
}

pub(crate) struct ExpectedLocalSource {
    pub path: PathBuf,
    // canonical in-root path plus inventoried identity and content hash
}

pub(crate) struct ExpectedRemoteDestination {
    pub snapshot: RemoteDestinationSnapshot,
}

pub(crate) enum ExpectedLocalDirectory {
    Missing,
    Directory {
        canonical_in_root: PathBuf,
    },
}

pub(crate) struct ExpectedDirectorySnapshots {
    pub relative: String,
    pub local: ExpectedLocalDirectory,
    // Missing or Directory when planning succeeds; File remains a type issue.
    pub remote: RemoteDestinationSnapshot,
}
~~~

Add crate-private functions with this conceptual shape:

~~~rust
pub(crate) fn download_one_guarded(
    state: &mut StateFile,
    local_path: &Path,
    rel: &str,
    remote: &RemoteHash,
    expected: &ExpectedLocalDestination,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<CommitDecision>;

pub(crate) fn upload_one_guarded<R: RemoteWrite>(
    remote: &mut R,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    bytes: &[u8],
    hash: &str,
    source: &ExpectedLocalSource,
    destination: &ExpectedRemoteDestination,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<CommitDecision>;
~~~

For scoped calls, staging APIs must not create local or remote parent
directories. Task 6 materializes every required parent through its own
`CommitGate.commit` before staging begins. If a parent disappears before
staging, fail safely; do not recreate it outside a claim. Legacy public wrappers
may retain their existing unconditional parent-materialization behavior.

Inside the final claimed closure:

- downloads revalidate the local parent and destination against
  `ExpectedLocalDestination` before rename;
- uploads revalidate `ExpectedLocalSource` (regular file, canonical path
  inside the root, identity and content hash) and
  `ExpectedRemoteDestination` immediately before remote rename;
- remote revalidation uses a crate-private fakeable snapshot method on
  `RemoteWrite`; the Ftp implementation performs strict parent discovery and
  hashes a present file rather than treating a probe error as absence;
- any absence, appearance, type change, identity change, or content change
  returns an error without destination rename or state update.

Dry-run returns Committed without staging or mutation. Keep existing public
upload_one/download_one behavior by delegating through
UnconditionalCommitGate or by leaving thin compatible wrappers. Do not alter
prepared Pull's one-shot authorization contract.

- [ ] **Step 6: Test final-claim ordering and cleanup**

Add tests for:

- cancellation before claim;
- claim before late invalidation;
- rename error cleans temp and does not update state;
- local download destination identity/type change before commit fails safely;
- local upload source disappearance, symlink/type change, identity change, and
  content-hash change before commit each fail safely;
- remote upload destination appearance, disappearance, type change, and
  content/metadata change before commit each fail safely;
- a missing staged-write parent fails without unguarded recreation;
- remote temp metadata is used in the committed record;
- no state record is inserted before a successful rename;
- existing preexisting temp protections still pass.

- [ ] **Step 7: Run existing transfer suites**

Run:

    cargo test commands::pull
    cargo test commands::push
    cargo test --test editor_sync_integration --no-run
    cargo test --test pull_integration --no-run
    cargo test --test push_integration --no-run

Expected: PASS.

- [ ] **Step 8: Commit**

    git add src/commands/sync/commit.rs src/commands/sync.rs src/commands/pull.rs src/commands/push.rs src/commands/file_transfer.rs
    git commit -m "refactor: add guarded transfer commits"

### Task 5: Implement the structured scoped sync engine

**Files:**
- Modify: src/commands/sync.rs
- Modify: src/commands/sync/inventory.rs
- Modify: src/commands/sync/commit.rs
- Test: src/commands/sync.rs
- Test: tests/cli_test.rs

- [ ] **Step 1: Write failing structured outcome tests**

Use typed output instead of printing:

~~~rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEventKind {
    Unchanged,
    Uploaded,
    Downloaded,
    CreatedLocalDirectory,
    CreatedRemoteDirectory,
    SkippedAbsent,
    ForcedRemoteOverwrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEvent {
    pub path: String,
    pub kind: SyncEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncIssue {
    FileConflict { path: String, state: FileState },
    TypeConflict {
        path: String,
        local: EntryKind,
        remote: EntryKind,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub events: Vec<SyncEvent>,
    pub issues: Vec<SyncIssue>,
    pub cancelled: bool,
}
~~~

Tests prove:

- local/remote hash combinations map to existing action semantics;
- BothChanged and Untracked become FileConflict without force;
- force maps those cases to ForcedRemoteOverwrite;
- a type mismatch is not treated as a file conflict;
- state-only absent entries become SkippedAbsent;
- event ordering is deterministic by relative path.
- a gate invalidated before the first transfer yields cancelled without staging;
- invalidation between entries preserves prior commits and does not stage the
  next entry;
- an all-unchanged scope invalidated during final validation yields cancelled,
  not a successful no-op.

- [ ] **Step 2: Run and verify RED**

Run:

    cargo test commands::sync::tests::structured

Expected: FAIL because structured types/planning do not exist.

- [ ] **Step 3: Separate core execution from CLI rendering**

Add:

~~~rust
pub fn run_scoped(
    config_path: &Path,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<SyncOutcome>;
~~~

Core rules:

- load config/state/matcher and connect once;
- complete strict inventory before any mutation;
- collect remote hashes/bytes and local hashes only for inventory files;
- reuse remote_hash stable retrieval and current classify;
- create a deterministic execution plan;
- execute clean entries sequentially;
- stop scheduling further mutations when CommitDecision::Cancelled;
- persist records for commits completed before conflict/cancellation;
- call `gate.is_current()` immediately before staging each upload/download or
  beginning each directory validation; if false, set `cancelled` and stop
  without creating another temp;
- after all entries, run a non-mutating final validation pass over the expected
  post-plan directory snapshots;
- check `gate.is_current()` both before and after that final pass, including
  all-unchanged scopes; an invalidated generation returns
  `SyncOutcome { cancelled: true, .. }` and must not be rendered as success;
- return typed issues instead of Exit from the shared API;
- never print.
- capture `ExpectedLocalDestination`, `ExpectedLocalSource`, and
  `ExpectedRemoteDestination`, plus expected local/remote directory
  snapshots, from the completed inventory/hashing phase;
- immediately before each destination rename, inside the acquired commit
  claim, revalidate every applicable source and destination snapshot;
- for upload, strict remote destination revalidation occurs before rename,
  while the gate is held, and any indeterminate listing/probe error is fatal;
- a target expected absent that appears, or a target expected present that
  disappears/changes, is a safety error rather than an overwrite/recreation;
- perform no network or filesystem check between a successful destination
  rename and the corresponding in-memory state update in the same closure;

- [ ] **Step 4: Add the CLI renderer and error mapping**

run_cli must:

- call the untouched legacy adapter for LegacyProject;
- call run_scoped for RootDirectory/Path with UnconditionalCommitGate;
- render each SyncEvent with existing wording where applicable:
  - uploaded PATH
  - downloaded PATH
  - would upload/download PATH in dry-run
  - created local directory PATH
  - created remote directory PATH
- render all issues after clean progress;
- return Exit::Conflict and process exit 2 when file conflicts exist;
- return a normal anyhow error and process exit 1 when type conflicts exist;
- keep configuration/auth exit 3;
- render no output from run_scoped itself.

- [ ] **Step 5: Add CLI compatibility tests**

In tests/cli_test.rs, prove:

- bare ferry sync still accepts no positional argument;
- explicit missing path returns nonzero with the defined not-found text;
- two paths and PATH plus --select fail at clap;
- type conflict maps to generic exit 1;
- file conflict maps to exit 2;
- no explicit path can accidentally invoke project-wide sync.

Use test-only/fake seams for non-network unit cases; Docker behavior belongs to Task 6.

- [ ] **Step 6: Run focused and full non-Docker tests**

Run:

    cargo test commands::sync
    cargo test --test cli_test
    cargo test --test sync_integration --no-run
    cargo test --test dry_run_integration --no-run
    rg -n "println!|eprintln!" src/commands/sync/inventory.rs src/commands/sync/scope.rs src/commands/sync/commit.rs

Expected: PASS and no prints in the shared scoped modules. Printing is allowed only in the CLI adapter in src/commands/sync.rs.

- [ ] **Step 7: Commit**

    git add src/commands/sync.rs src/commands/sync/inventory.rs src/commands/sync/commit.rs src/main.rs tests/cli_test.rs
    git commit -m "feat: execute structured scoped sync"

### Task 6: Materialize recursive and empty directories safely

**Files:**
- Modify: src/commands/sync.rs
- Modify: src/commands/sync/inventory.rs
- Modify: src/commands/push.rs
- Modify: src/commands/pull.rs
- Create: tests/scoped_sync_integration.rs
- Modify: tests/dry_run_integration.rs

- [ ] **Step 1: Write ignored Docker tests before implementation**

In tests/scoped_sync_integration.rs, reuse tests/support/mod.rs. Add independent tests for:

1. remote-only "zones/new/" with files and an empty child downloads beneath the selected root;
2. local-only "assets/new/" with files and an empty child uploads beneath the selected root;
3. one exact file sync leaves changed siblings untouched;
4. selected "area/" leaves "area-old/" untouched;
5. explicit configured-root path works and materializes empty directories;
6. a file/directory mismatch preserves both sides and exits 1;
7. a file conflict exits 2 while clean entries in the same selected subtree complete and state is saved;
8. stale state-only entries are reported without deletion;
9. --force remains local-wins for an explicit CLI path;
10. bare no-argument sync retains existing behavior.

Also add deterministic fake-remote/unit tests in src/commands/sync.rs:

- a local-only directory disappears before remote creation;
- a local-only directory becomes a file before remote creation;
- a remote-only directory disappears before local creation;
- a remote-only directory becomes a file before local creation;
- a missing local or remote destination appears before its create commit;
- a shared local directory changes type before final validation;
- a shared remote directory changes type before final strict validation;
- an empty all-unchanged directory scope is invalidated during final
  validation and reports cancellation;
- strict remote MKD returning 550/error is propagated, with no tolerant list
  fallback;
- every race leaves the counterpart untouched.

- [ ] **Step 2: Compile the Docker tests and verify behavioral RED**

Run:

    cargo test --test scoped_sync_integration --no-run
    cargo test --test scoped_sync_integration -- --ignored --nocapture --test-threads=1
    cargo test commands::sync::tests::directory_race

Expected: compilation passes; scoped behavior and directory-race tests FAIL.

- [ ] **Step 3: Implement top-down directory planning**

Before file transfers:

- calculate missing local and remote directories, including ancestors required by an exact file;
- sort directories by depth then name so parents precede children;
- capture the local presence/type/canonical-in-root snapshot and strict remote
  presence/type snapshot for every directory entry before mutation;
- immediately before each directory commit, call `gate.is_current()`; then,
  inside `CommitGate.commit`, revalidate both the source directory and missing
  destination against those snapshots;
- create local destinations with one `fs::create_dir`; create remote
  destinations only with `RemoteWrite::mkdir_scoped_strict`;
- never call legacy `Ftp::mkdir` or its tolerant LIST fallback from a scoped
  directory operation;
- record CreatedLocalDirectory/CreatedRemoteDirectory only after success;
- in dry-run emit events without mutation;
- when the gate cancels, stop immediately and preserve prior progress;
- never remove an existing directory.
- these directory commits finish before the related file is staged;
- if a committed parent disappears before staging or final revalidation, the
  transfer fails safely and the staging layer never recreates it outside a
  claim;
- after a successful create, update the expected post-plan snapshot to
  Directory; do not reuse the original Missing snapshot for final validation;
- after all mutations/no-ops, strictly revalidate every expected local and
  remote directory, including empty/shared directories, between the engine's
  final two `gate.is_current()` checks;
- any disappearance, appearance, canonical escape, or file/directory type
  change is a safety error/cancellation outcome and is never reported as
  successful sync.


- [ ] **Step 4: Handle empty directories and type conflicts**

Requirements:

- inventory directories survive even when they contain no files;
- no state record is created for a directory;
- file versus directory at the same relative path becomes SyncIssue::TypeConflict;
- descendants blocked by an ancestor type conflict are skipped without touching either side;
- unrelated clean entries may still complete, but the CLI exits 1 after reporting type issues.
- a shared directory is not treated as a permanent no-op: its local and remote
  types are rechecked in the final validation pass before success feedback;
- no directory create or revalidation writes a directory state-file record.

- [ ] **Step 5: Add scoped dry-run tests**

In tests/dry_run_integration.rs prove explicit file, explicit directory, RootDirectory, and empty-directory operations change no:

- local bytes;
- local directory structure;
- remote bytes;
- remote directory structure;
- Ferry state bytes.

- [ ] **Step 6: Run Docker suites**

Run:

    cargo test --test scoped_sync_integration -- --ignored --nocapture --test-threads=1
    cargo test --test sync_integration -- --ignored --nocapture --test-threads=1
    cargo test --test dry_run_integration -- --ignored --nocapture --test-threads=1

Expected: PASS.

- [ ] **Step 7: Commit**

    git add src/commands/sync.rs src/commands/sync/inventory.rs src/commands/push.rs src/commands/pull.rs tests/scoped_sync_integration.rs tests/dry_run_integration.rs
    git commit -m "feat: sync recursive file and directory scopes"

### Task 7: Add the interactive single-path terminal picker

**Files:**
- Create: src/commands/sync/picker.rs
- Modify: src/commands/sync.rs
- Modify: src/main.rs
- Modify: tests/cli_test.rs
- Test: src/commands/sync/picker.rs

- [ ] **Step 1: Write failing picker model and navigation tests**

Use injected boundaries:

~~~rust
pub trait PickerSource {
    fn list(&mut self, directory: &str) -> Result<Vec<PickerEntry>>;
}

pub trait PickerIo {
    fn read_line(&mut self, prompt: &str) -> Result<String>;
    fn write_line(&mut self, line: &str) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerEntry {
    pub name: String,
    pub kind: EntryKind,
    pub presence: Presence,
}
~~~

Tests must cover:

- merged alphabetical local/remote children;
- labels local, remote, and both;
- choosing one file returns SyncScope::Path;
- entering a remote-only directory then choosing "Sync this folder";
- choosing root "Sync this folder" returns RootDirectory;
- parent navigation never escapes root;
- cancel returns None and produces no sync call;
- one selection only;
- malformed numeric input re-prompts;
- control characters in a remote name are escaped/sanitized in output;
- ignored entries are absent.

- [ ] **Step 2: Run and verify RED**

Run:

    cargo test commands::sync::picker::tests

Expected: FAIL because picker does not exist.

- [ ] **Step 3: Implement the line-oriented picker**

Menu rules:

- display current normalized path;
- entry 0 is "Sync this folder";
- root has no parent item; nested directories have one parent item;
- selecting a directory browses into it;
- selecting a file returns it;
- include an explicit Cancel item;
- lazily list only the current directory;
- use strict remote listing and complete local read_dir for that level;
- merge by trusted relative child name;
- sanitize display only; retain original safe name for the selected path;
- revalidate through run_scoped after the picker closes.

Do not add dialoguer, crossterm, or a full-screen UI.

- [ ] **Step 4: Enforce interactive terminals in the real adapter**

Use std::io::IsTerminal. Real --select requires stdin and stdout to be terminals. Injected unit IO bypasses this check.

A non-interactive invocation must return:

    ferry sync --select requires an interactive terminal; pass PATH directly

Cancellation returns success without state, directory, or transfer writes.

- [ ] **Step 5: Wire selector into run_cli**

- load the nearest config from the task working directory;
- browse relative to that config's local/remote roots;
- return exactly one SyncScope;
- pass --force and --dry-run through after selection;
- never reinterpret cancellation as LegacyProject.

- [ ] **Step 6: Run picker and CLI tests**

Run:

    cargo test commands::sync::picker::tests
    cargo test --test cli_test
    cargo test --bin ferry

Expected: PASS.

- [ ] **Step 7: Commit**

    git add src/commands/sync/picker.rs src/commands/sync.rs src/main.rs tests/cli_test.rs
    git commit -m "feat: add interactive scoped sync picker"

### Task 8: Add reusable editor-scope invalidation and commit claims

**Files:**
- Modify: src/lsp/document_state.rs
- Modify: src/commands/sync/commit.rs
- Modify: src/commands/pull.rs
- Modify: src/commands/push.rs
- Test: src/lsp/document_state.rs
- Test: src/lsp.rs

- [ ] **Step 1: Write failing dirty-scope and lifecycle tests**

Add:

~~~rust
pub(crate) enum DocumentScope {
    Exact(PathBuf),
    Directory(PathBuf),
}

pub(crate) struct ScopeOperationGuard {
    // shared atomic state
}
~~~

DocumentTracker::begin_clean_scope must:

- reject a dirty tracked exact file;
- reject any dirty tracked descendant of a directory;
- accept a scope with no dirty tracked descendants;
- use path segment boundaries, so "/area" does not include "/area-old";
- register the scope globally so a newly opened descendant invalidates it;
- invalidate on didOpen, didChange, didSave, didClose, and cancel_all/shutdown;
- leave lifecycle events outside the scope alone.

- [ ] **Step 2: Write failing reusable atomic-claim tests**

Use atomic bits rather than a mutex held by the protocol thread:

- ACTIVE;
- COMMITTING;
- INVALIDATED;
- COMMITTING | INVALIDATED.

Required behavior:

1. claim from ACTIVE wins and invokes one mutation;
2. invalidation from ACTIVE prevents the claim;
3. invalidation during COMMITTING returns immediately, marks the invalid bit, and allows the already claimed mutation to finish;
4. releasing a clean claim returns to ACTIVE for the next file;
5. releasing a claim with invalid bit leaves INVALIDATED so every later claim is cancelled;
6. two concurrent claims cannot run together;
7. dropping DocumentTracker invalidates all scopes.

ScopeOperationGuard implements CommitGate. The unconditional CLI gate remains separate.

- [ ] **Step 3: Run and verify RED**

Run:

    cargo test lsp::document_state::tests::scope
    cargo test lsp::document_state::tests::commit

Expected: FAIL because scope registrations and reusable claims do not exist.

- [ ] **Step 4: Implement global scope registrations**

DocumentTracker gains a pruned list of:

~~~rust
struct ScopeRegistration {
    scope: DocumentScope,
    state: Weak<AtomicU8>,
}
~~~

Every successful lifecycle method first invalidates matching registered scopes, including didOpen before replacing/adding the DocumentEntry. begin_clean_scope checks dirty entries, prunes dead/invalid registrations, creates one shared state, and registers the exact/directory scope.

Keep existing one-file OperationGuard behavior unchanged for Pull, Compare, and Force Pull.

- [ ] **Step 5: Add deterministic post-staging denial tests**

Use the crate-private local and remote staging seams from Task 4:

- stage a local replacement, pause, call tracker.change/save/cancel_all, release, and assert CommitDecision::Cancelled, original destination, unchanged state, and temp cleanup;
- stage a remote replacement in a fake, perform the same three invalidation cases, and assert rename/state never happen and remote temp cleanup is attempted;
- separately claim first, invalidate while COMMITTING, and prove the claimed commit finishes but the next commit is cancelled.
- run a two-entry scope, invalidate after the first committed entry, and prove
  the first state change persists while no temp/staging call begins for the
  second entry;
- pause an all-unchanged/empty-directory scope in final validation, invalidate
  it, and prove the outcome is cancelled, no mutation/temp occurs, and LSP
  emits cancellation feedback rather than success.

Use recv_timeout for every synchronization channel; never add unbounded recv calls.

- [ ] **Step 6: Run focused concurrency tests repeatedly**

Run:

    cargo test lsp::document_state::tests -- --test-threads=1
    cargo test lsp::tests::scope_commit -- --test-threads=1

Expected: both deterministic test groups PASS without hangs or timeouts.

- [ ] **Step 7: Commit**

    git add src/lsp/document_state.rs src/commands/sync/commit.rs src/commands/pull.rs src/commands/push.rs src/lsp.rs
    git commit -m "feat: guard scoped sync commits from editor changes"

### Task 9: Expose Sync Current File and Sync Current Folder in ferry-lsp

**Files:**
- Modify: src/lsp.rs
- Modify: src/lsp/document_state.rs
- Modify: tests/editor_sync_integration.rs
- Test: src/lsp.rs

- [ ] **Step 1: Write failing action ordering and capability tests**

Add constants:

~~~rust
pub const SYNC_FILE_COMMAND: &str = "ferry.syncFile";
pub const SYNC_FOLDER_COMMAND: &str = "ferry.syncFolder";
~~~

Expected Code Action order:

1. Ferry: Pull
2. Ferry: Compare with Remote
3. Ferry: Force Pull (overwrite local)
4. Ferry: Push
5. Ferry: Sync Current File
6. Ferry: Sync Current Folder
7. Ferry: Compile-check

Update tests to assert exact titles, command IDs, URI arguments, ACTION_COMMANDS, and ExecuteCommandOptions order.

- [ ] **Step 2: Run and verify RED**

Run:

    cargo test lsp::tests::capabilities_advertise
    cargo test lsp::tests::code_actions

Expected: FAIL at five versus seven actions.

- [ ] **Step 3: Extend the operation boundary**

Add a structured request:

~~~rust
pub struct SyncRequest {
    pub config_path: PathBuf,
    pub scope: SyncScope,
    pub gate: Arc<dyn CommitGate>,
}

pub trait FileOperations {
    // existing methods...
    fn sync(&mut self, request: SyncRequest) -> Result<SyncOutcome>;
}
~~~

FerryOperations calls run_scoped with ExecutionMode::Apply and no force. Fake operations record the scope and exercise the supplied gate where needed.

The shared engine returns data only. LSP code must never invoke run_cli or any renderer.

- [ ] **Step 4: Derive exact file and parent-directory scopes**

At execute-command acknowledgement time:

- resolve the active document to the nearest Ferry project;
- Sync Current File uses SyncScope::Path(relative file);
- Sync Current Folder uses its parent:
  - parent empty becomes RootDirectory;
  - otherwise Path(parent) and inventory determines directory;
- create DocumentScope::Exact or Directory from canonical absolute paths;
- call begin_clean_scope before enqueueing;
- on a dirty descendant, acknowledge once and emit "save all files in this folder and retry";
- re-resolve config/root in the worker before execution, matching existing queued-work safety.

- [ ] **Step 5: Preserve worker responsiveness and serialization**

The existing single operation worker already serializes mutating LSP work. Keep that property explicit:

- sync runs only on the worker;
- protocol loop continues processing Code Actions and document notifications;
- didOpen/change/save/close invalidates the ScopeOperationGuard immediately;
- a queued automatic save runs only after folder sync returns and re-resolves config/state;
- shutdown sets running false, invalidates all scope guards, closes the LSP writer promptly, and prevents later worker feedback.

- [ ] **Step 6: Render typed LSP feedback**

Add a sync_feedback function:

- successful events summarize counts in one Info notification;
- file/type conflicts show Warning with "conflict; run a Ferry task for details";
- cancelled scope shows Warning with "folder changed in Zed; save all files and retry";
- config/auth/generic errors retain redacted safe_error_summary;
- never include host, user, password, remote_root, absolute snapshot paths, or raw FTP listing text.

- [ ] **Step 7: Add deterministic protocol tests**

Using Connection::memory and recv_timeout, prove:

- both new commands acknowledge before worker completion;
- current file passes the exact scope;
- a root-level file's folder action passes RootDirectory;
- a nested file passes only its parent directory;
- a dirty current file refuses both actions;
- a dirty sibling below the folder refuses the folder action;
- a dirty sibling outside the folder does not refuse it;
- didOpen, didChange, didSave, and didClose beneath an in-flight folder scope cancel before the next commit;
- shutdown after staging prevents destination replacement/state update and returns promptly;
- a queued save re-resolves after folder cancellation;
- Code Actions remain responsive while folder sync blocks;
- all responses and notifications are valid LSP messages with no raw sync output.

- [ ] **Step 8: Add real-FTP no-stdout coverage**

In tests/editor_sync_integration.rs, add an ignored Docker test that starts the ferry-lsp binary over stdio, sends framed initialize/didOpen/executeCommand/shutdown messages for scoped sync, and parses every stdout byte as Content-Length-framed JSON-RPC. Fail if "uploaded", "downloaded", conflict text, or any unframed byte appears outside JSON payloads.

- [ ] **Step 9: Run LSP and Docker tests**

Run:

    cargo test lsp::tests
    cargo test --test editor_sync_integration --no-run
    cargo test --test editor_sync_integration -- --ignored --nocapture --test-threads=1

Expected: PASS.

- [ ] **Step 10: Commit**

    git add src/lsp.rs src/lsp/document_state.rs tests/editor_sync_integration.rs
    git commit -m "feat: add scoped sync Zed actions"

### Task 10: Add Zed tasks, documentation, full verification, and controlled GUI smoke

**Files:**
- Modify: examples/tasks.json
- Modify: README.md
- Modify: extensions/ferry/README.md
- Modify: extensions/ferry/extension.toml
- Verify: all files above and the complete branch

- [ ] **Step 1: Write the three example tasks**

Add these shapes without force:

~~~json
{
  "label": "Ferry: sync current file",
  "command": "ferry",
  "args": ["sync", "$ZED_FILE"],
  "cwd": "$ZED_DIRNAME",
  "use_new_terminal": false,
  "reveal": "always",
  "hide": "on_success"
},
{
  "label": "Ferry: sync current folder",
  "command": "ferry",
  "args": ["sync", "$ZED_DIRNAME"],
  "cwd": "$ZED_DIRNAME",
  "use_new_terminal": false,
  "reveal": "always",
  "hide": "on_success"
},
{
  "label": "Ferry: choose path to sync...",
  "command": "ferry",
  "args": ["sync", "--select"],
  "cwd": "$ZED_DIRNAME",
  "use_new_terminal": false,
  "reveal": "always"
}
~~~

Keep project-wide sync and existing safe tasks. Do not add a force task.

- [ ] **Step 2: Validate task JSON**

Run:

    python -m json.tool examples/tasks.json

Expected: formatted JSON on stdout and exit 0.

- [ ] **Step 3: Update root and extension documentation**

Document all approved behavior:

- literal Project Panel right-click actions are unavailable through Zed's extension API;
- Ctrl+. now shows seven actions in exact order;
- current-folder sync is recursive;
- terminal tasks must be run after Save All because they cannot inspect dirty Zed buffers;
- choose-path uses one selection and can browse remote-only roots;
- bare sync remains project-wide;
- sync PATH accepts one file/directory;
- --select and PATH are exclusive;
- --force is explicit CLI-only local-wins behavior;
- --dry-run is non-mutating;
- remote-only/local-only and empty directories are created;
- no sync operation propagates deletion;
- conflicts and partial clean progress;
- named per-project task example such as ferry sync areas;
- no automatic directory sync;
- pull_on_open and push_on_save remain false by default;
- the extension remains attached to C unless users add languages.

Update extension.toml description only; do not change language attachment or schema.

- [ ] **Step 4: Run formatting, static checks, and all non-Docker tests**

Run exactly:

    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets
    cargo test --doc
    python -m json.tool examples/tasks.json
    cargo test --manifest-path extensions/ferry/Cargo.toml
    cargo check --manifest-path extensions/ferry/Cargo.toml --target wasm32-wasip2
    git diff --check

Expected: every command exits 0.

- [ ] **Step 5: Run all Docker FTP suites serially**

Run:

    cargo test --test scoped_sync_integration -- --ignored --nocapture --test-threads=1
    cargo test --test sync_integration -- --ignored --nocapture --test-threads=1
    cargo test --test pull_integration -- --ignored --nocapture --test-threads=1
    cargo test --test push_integration -- --ignored --nocapture --test-threads=1
    cargo test --test dry_run_integration -- --ignored --nocapture --test-threads=1
    cargo test --test editor_sync_integration -- --ignored --nocapture --test-threads=1

Expected: all tests PASS. Record exact counts in the handoff.

- [ ] **Step 6: Commit documentation**

    git add examples/tasks.json README.md extensions/ferry/README.md extensions/ferry/extension.toml
    git commit -m "docs: document scoped Ferry sync"

- [ ] **Step 7: Request independent code review**

Use @superpowers:requesting-code-review against the range from c118191 through the final implementation commit. Require the reviewer to check:

- no Critical or Important findings;
- literal spec alignment;
- malformed LIST fail-closed behavior;
- root-scope distinction;
- no LSP stdout printing;
- atomic staged commit claims;
- mid-flight lifecycle and shutdown tests;
- legacy sync/pull/push compatibility;
- credential and live-project scope audit.

Fix any Critical/Important issue with TDD, rerun affected/full verification, and request a final review.

- [ ] **Step 8: Perform a controlled Zed smoke test**

Only after automated review is clean:

1. Start a disposable Docker FTP fixture with local-only, remote-only, shared, empty, and conflicting directories.
2. Create a fresh disposable Zed project outside the live 3S tree.
3. Build/install ferry and ferry-lsp from this feature worktree.
4. Launch a new Zed workspace with the worktree target/debug directory first on PATH.
5. Confirm seven Code Actions are visible.
6. Run Sync Current File.
7. Run Sync Current Folder recursively.
8. Run choose-path and select a remote-only top-level folder.
9. Confirm empty descendants appear locally.
10. Cancel the picker and confirm no mutation.
11. Create a conflict and confirm normal sync refuses with Warning.
12. Edit/save a descendant while folder sync is paused and confirm the destination is not replaced after staging.
13. Confirm no raw terminal text or protocol warnings appear in Zed logs.
14. Stop/remove only the disposable container and project after Zed closes.

Do not kill the existing /home/simon/.cargo/bin/ferry-lsp process and do not exercise /home/simon/code/3s/.ferry.toml.

- [ ] **Step 9: Final branch and PR handoff**

Run:

    git status --short --branch
    git log --oneline --decorate -12
    git diff --check c118191..HEAD

Expected: clean worktree, implementation commits present, no patch errors.

Push the feature branch only after verification and update the existing PR #2 summary/checklist with scoped sync behavior and test evidence. Do not merge without the user's direction.
