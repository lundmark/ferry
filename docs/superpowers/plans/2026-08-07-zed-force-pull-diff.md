# Zed Force Pull and Remote Diff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add safe, save-first Zed Code Actions for comparing a Ferry file with its remote version and explicitly force-pulling that remote version after confirmation.

**Architecture:** Refactor single-file Pull into read/commit phases so network preparation never implies a write. Keep editor revision guards and LSP request correlation in focused LSP modules, and isolate temporary snapshots plus `zed --diff` launching behind a testable runtime boundary. The protocol loop stays responsive while its worker performs FTP and filesystem work.

**Tech Stack:** Rust 2024, Cargo, `lsp-server` 0.8, `lsp-types` 0.97, `tempfile`, `fs2`, Zed CLI, existing Docker-backed FTP integration fixtures

---

## Execution constraints

- Work only in `/home/simon/code/zed_ftp/.worktrees/zed-code-actions-keybinding` until branch completion.
- Before each production change, invoke `@superpowers:test-driven-development` and follow RED-GREEN-REFACTOR.
- Never read, print, copy, or modify the live `/home/simon/code/3s/.ferry.toml` credentials.
- Never run Pull, Push, Force Pull, Compare, or Compile-check against the live 3S remote during automated verification.
- Use only the existing Docker FTP fixture or fakes for networked tests.
- Do not stage or delete the generated `extensions/ferry/extension.wasm` in the durable main worktree.
- Preserve the existing CLI `ferry pull --force` result and output behavior.
- Run `@superpowers:verification-before-completion` before claiming completion and `@superpowers:requesting-code-review` before branch integration.

## File structure

| File | Responsibility |
|---|---|
| `src/config.rs` | Project editor-sync defaults; both automatic actions become opt-in. |
| `src/commands/pull.rs` | Whole-tree Pull and shared atomic-write/state helpers; re-exports prepared single-file APIs. |
| `src/commands/pull/prepared.rs` | Read-only retrieval, identities, prepared normal/force pulls, guarded installation, compatible `pull_one`. |
| `src/lsp/document_state.rs` | Dirty state, revisions, and one-shot operation guards cancelled by edits. |
| `src/lsp/diff.rs` | Private snapshot roots, stale-root locking/cleanup, snapshots, and `zed --diff` launching. |
| `src/lsp.rs` | Code Actions, command parsing, worker events, confirmation correlation, and feedback. |
| `src/bin/ferry-lsp.rs` | Construct the fallible production LSP runtime. |
| `tests/editor_sync_integration.rs` | Docker proof that retrieval is read-only and force installation is guarded. |
| `README.md`, `extensions/ferry/README.md` | Defaults and five-action Zed workflow. |
| `Cargo.toml`, `Cargo.lock` | Runtime temporary-file and file-lock dependencies. |

Do not add project-local snapshots, a new `.ferry.toml` Zed key, a Ferry CLI diff command, or extension-side UI.

### Task 1: Make automatic editor sync opt-in

**Files:**
- Modify: `src/config.rs:43-56,122-163`
- Modify: `src/lsp.rs:811-853`

- [ ] **Step 1: Run the baseline non-live suite**

Run:

```sh
cargo test
```

Expected: all non-ignored tests pass; Docker tests stay ignored.

- [ ] **Step 2: Write the failing default tests**

Rename `editor_defaults_preserve_pull_and_disable_push` to
`editor_defaults_disable_automatic_sync` and assert:

```rust
assert_eq!(
    cfg.editor,
    Editor {
        pull_on_open: false,
        push_on_save: false,
    }
);
```

Change the LSP default-open test to expect no operation. Change the explicit
enabled test to use:

```rust
let fixture = Fixture::new("[editor]\npull_on_open = true\n");
```

and expect one non-force Pull.

- [ ] **Step 3: Verify RED**

Run:

```sh
cargo test editor_defaults_disable_automatic_sync
```

Expected: FAIL because `pull_on_open` is still true.

- [ ] **Step 4: Implement the false defaults**

Use:

```rust
#[derive(Debug, Deserialize, PartialEq, Default)]
pub struct Editor {
    #[serde(default)]
    pub pull_on_open: bool,
    #[serde(default)]
    pub push_on_save: bool,
}
```

Remove the manual `Default` implementation. Keep `default_true` for
`connection.passive`.

- [ ] **Step 5: Verify GREEN and regressions**

```sh
cargo test editor_defaults_disable_automatic_sync
cargo test automatic_open
cargo test
```

Expected: all pass.

- [ ] **Step 6: Commit**

```sh
git add src/config.rs src/lsp.rs
git commit -m "fix: make automatic editor sync opt-in"
```

### Task 2: Add prepared single-file Pull primitives

**Files:**
- Create: `src/commands/pull/prepared.rs`
- Modify: `src/commands/pull.rs:1-22,253-452`

- [ ] **Step 1: Add the module and failing local-safety tests**

Declare/re-export in `pull.rs`:

```rust
mod prepared;

pub use prepared::{
    LocalIdentity, PreparedPull, RemoteFile, apply_prepared_pull,
    fetch_remote_one, prepare_force_pull_one, prepare_pull_one, pull_one,
};
```

In the new module, write tests named:

```rust
fn local_identity_distinguishes_missing_and_present_files()
fn apply_rejects_a_changed_local_identity_without_writing_state()
fn apply_installs_bytes_and_records_supplied_remote_metadata()
```

Construct `PreparedPull` directly in module tests. Assert exact local and
serialized state bytes around failure cases.

- [ ] **Step 2: Verify RED**

```sh
cargo test commands::pull::prepared::tests
```

Expected: compile failure because the types/functions do not exist.

- [ ] **Step 3: Implement identities and prepared data**

Use these shapes:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalIdentity {
    Missing,
    Present(String),
}

impl LocalIdentity {
    pub fn capture(path: &Path) -> Result<Self> {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                Ok(Self::Present(crate::hash::hash_file(path)?))
            }
            Ok(_) => anyhow::bail!("{} is not a regular file", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemoteFile {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub size: u64,
    pub mtime: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug)]
enum PreparedAction {
    Noop(TransferStatus),
    Install(RemoteFile),
}

#[derive(Clone, Debug)]
pub struct PreparedPull {
    config_path: PathBuf,
    local_root: PathBuf,
    local_path: PathBuf,
    relative_path: String,
    expected_local: LocalIdentity,
    action: PreparedAction,
}
```

Expose only read-only accessors needed by the LSP.

- [ ] **Step 4: Implement guarded application**

`apply_prepared_pull` must:

1. reload config and reject a changed `local_root`;
2. recapture `LocalIdentity` and return `Exit::Conflict` on mismatch;
3. return stored no-op status without mutation; or
4. in Apply mode, load state, atomically write bytes, record supplied
   hash/size/mtime plus `Utc::now()`, and save state.

Extract a metadata-only helper in `pull.rs`:

```rust
fn record_download(
    state: &mut StateFile,
    rel: &str,
    new_hash: &str,
    size: u64,
    remote_mtime: DateTime<Utc>,
) {
    state.files.insert(rel.to_string(), FileRecord {
        sha256: new_hash.to_string(),
        size,
        remote_mtime,
        last_synced: Utc::now(),
    });
}
```

Have existing `download_one` call this helper after obtaining mtime. Keep
`write_local_atomic` as the only local write.

- [ ] **Step 5: Put `pull_one` behind prepare/apply**

Move single-file logic into `prepared.rs`, leaving:

```rust
pub fn pull_one(
    config_path: &Path,
    rel: &str,
    force: bool,
    mode: ExecutionMode,
) -> Result<TransferOutcome> {
    apply_prepared_pull(prepare_pull_one(config_path, rel, force)?, mode)
}
```

Do not change whole-tree `pull::run`, `sync`, or `init` semantics.

- [ ] **Step 6: Verify and commit**

```sh
cargo test commands::pull::prepared::tests
cargo test
git add src/commands/pull.rs src/commands/pull/prepared.rs
git commit -m "refactor: prepare single-file pulls before commit"
```

Expected: pure tests and non-live suite pass.

### Task 3: Prove read-only retrieval and guarded force installation

**Files:**
- Modify: `src/commands/pull/prepared.rs`
- Modify: `tests/editor_sync_integration.rs`

- [ ] **Step 1: Write ignored Docker tests**

Add:

```rust
#[test]
#[ignore]
fn fetch_remote_one_returns_bytes_without_mutating_local_or_state()

#[test]
#[ignore]
fn prepared_force_pull_installs_the_fetched_remote_and_updates_state()

#[test]
#[ignore]
fn prepared_force_pull_rejects_a_local_change_after_preparation()

#[test]
#[ignore]
fn prepared_force_pull_requires_a_remote_file()
```

Use existing fixture helpers. The retrieval test compares local and
`.ferry/state.json` bytes before/after.

- [ ] **Step 2: Verify RED with controlled fixtures**

```sh
cargo test --test editor_sync_integration fetch_remote_one_returns_bytes_without_mutating_local_or_state -- --ignored --exact
cargo test --test editor_sync_integration prepared_force_pull -- --ignored
```

Expected: missing/incomplete retrieval behavior.

- [ ] **Step 3: Implement `fetch_remote_one`**

It must validate `safe_rel` before connecting, load config, connect with
existing fields, require remote presence, download once, compute `hash_bytes`,
capture size/mtime, and return `RemoteFile`. It must never load/save state or
touch the local path.

- [ ] **Step 4: Implement normal and forced preparation**

Preserve normal classification:

```text
InSync                     -> Noop(Unchanged)
LocalOnly                  -> Noop(SkippedMissingSource)
RemoteOnly/RemoteChanged   -> Install
conflict without force     -> Exit::Conflict
conflict with force        -> Install
```

`prepare_force_pull_one` always calls `fetch_remote_one`, captures local
identity, and returns Install even for equal hashes. A missing remote is an
error. This ensures the Zed action confirms a real overwrite.

- [ ] **Step 5: Verify controlled integration and CLI compatibility**

```sh
cargo test --test editor_sync_integration fetch_remote_one_returns_bytes_without_mutating_local_or_state -- --ignored --exact
cargo test --test editor_sync_integration prepared_force_pull -- --ignored
cargo test --test editor_sync_integration single_file_force_resolves_both_conflict_directions -- --ignored --exact
cargo test --test pull_integration pull_refuses_local_changed_without_force_and_obeys_force -- --ignored --exact
cargo test
```

Expected: controlled Docker cases and non-live tests pass; CLI force Pull stays
compatible.

- [ ] **Step 6: Commit**

```sh
git add src/commands/pull/prepared.rs tests/editor_sync_integration.rs
git commit -m "feat: prepare guarded remote file installs"
```

### Task 4: Track dirty documents and cancel pending operations

**Files:**
- Create: `src/lsp/document_state.rs`
- Modify: `src/lsp.rs:1-28`

- [ ] **Step 1: Write failing guard tests**

Declare `mod document_state;`. Test matching/differing didOpen text, claim
exactly once, didChange cancelling all same-file guards, didSave allowing only
new guards, didClose cancellation/removal, and other-file independence.

- [ ] **Step 2: Verify RED**

```sh
cargo test lsp::document_state::tests
```

Expected: missing `DocumentTracker` and `OperationGuard`.

- [ ] **Step 3: Implement the one-shot guard**

```rust
const PENDING: u8 = 0;
const CANCELLED: u8 = 1;
const CLAIMED: u8 = 2;

#[derive(Clone)]
pub(crate) struct OperationGuard {
    state: Arc<AtomicU8>,
    revision: u64,
}

impl OperationGuard {
    pub(crate) fn try_claim(&self) -> bool {
        self.state
            .compare_exchange(PENDING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}
```

Tracker entries keep weak references so completed operations do not leak.

- [ ] **Step 4: Implement lifecycle methods**

```rust
pub(crate) fn open(&mut self, path: PathBuf, text: &str) -> Result<()>;
pub(crate) fn change(&mut self, path: &Path);
pub(crate) fn save(&mut self, path: &Path);
pub(crate) fn close(&mut self, path: &Path);
pub(crate) fn begin_clean_operation(
    &mut self,
    path: &Path,
) -> Result<OperationGuard, DocumentStateError>;
```

`open` compares text bytes with disk. `change` increments revision, marks
dirty, and cancels PENDING guards. `save` increments revision and marks clean.
`close` cancels/removes. Missing tracking is an error.

- [ ] **Step 5: Verify and commit**

```sh
cargo test lsp::document_state::tests
cargo test
git add src/lsp.rs src/lsp/document_state.rs
git commit -m "feat: guard Ferry actions against dirty documents"
```

### Task 5: Build the private snapshot and Zed launcher runtime

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`
- Create: `src/lsp/diff.rs`
- Modify: `src/lsp.rs:1-28`

- [ ] **Step 1: Add runtime dependencies**

Move `tempfile = "3"` to `[dependencies]` and add `fs2 = "0.4"`. Run
`cargo check` to update the lock. Request network approval if needed; do not
replace locking with unsafe broad cleanup.

- [ ] **Step 2: Write failing snapshot/launcher tests**

Test unique safe suffixes, exact bytes, Unix read-only mode, traversal
containment, unretained deletion, retained lifetime, unlocked stale-root
cleanup, locked-root preservation, and exact command arguments
`zed --diff local remote`.

- [ ] **Step 3: Verify RED**

```sh
cargo test lsp::diff::tests
```

Expected: missing snapshot store/launcher.

- [ ] **Step 4: Implement safe roots and snapshots**

Create a `tempfile::TempDir` with exact prefix `ferry-lsp-diff-v1-`. Create
and exclusively lock `.lock` for store lifetime. On startup, scan only direct
temp-root children with that prefix; reject symlinks/non-directories, and
remove a candidate only after exclusively locking its `.lock`. Never remove
the platform temp root or an unresolved path.

Use `NamedTempFile` under the locked root with `remote-` plus a sanitized
alphanumeric extension. Flush bytes, make read-only on Unix, return an owning
`PreparedSnapshot`, retain only after successful launch.

- [ ] **Step 5: Implement the launcher seam**

```rust
pub(crate) trait DiffLauncher: Send {
    fn launch(&mut self, local: &Path, remote: &Path) -> Result<()>;
}

pub(crate) struct ZedDiffLauncher;

impl DiffLauncher for ZedDiffLauncher {
    fn launch(&mut self, local: &Path, remote: &Path) -> Result<()> {
        std::process::Command::new("zed")
            .arg("--diff")
            .arg(local)
            .arg(remote)
            .spawn()
            .with_context(|| "launching Zed native diff")?;
        Ok(())
    }
}
```

Factor command construction for no-GUI argument tests.

- [ ] **Step 6: Verify and commit**

```sh
cargo test lsp::diff::tests
cargo test
cargo fmt --check
git add Cargo.toml Cargo.lock src/lsp.rs src/lsp/diff.rs
git commit -m "feat: prepare private Zed diff snapshots"
```

### Task 6: Wire dirty lifecycle into the LSP

**Files:**
- Modify: `src/lsp.rs:9-198,331-474,570-2006`

- [ ] **Step 1: Write failing notification/capability tests**

Add didChange/didClose helpers. Expect `TextDocumentSyncKind::INCREMENTAL`.
Add memory-loop tests proving matching open permits Pull, differing open and
didChange refuse without operations, didSave permits a new Pull, and didClose
makes stale commands fail safely.

- [ ] **Step 2: Verify RED**

```sh
cargo test capabilities_advertise_text_sync_and_exact_ferry_actions
cargo test dirty_document
```

- [ ] **Step 3: Update notification handling**

Instantiate `DocumentTracker` in `protocol_loop`, updating synchronously:

```text
didOpen   -> tracker.open(path, text), queue Open with optional clean guard
didChange -> tracker.change(path), no FTP work
didSave   -> tracker.save(path), queue Save
didClose  -> tracker.close(path), no FTP work
```

Malformed/non-file notifications never clear dirty state. Worker still
re-resolves project/config before automatic work.

- [ ] **Step 4: Guard manual and automatic Pull acceptance**

After URI validation, guarded commands call `begin_clean_operation`. On
failure, acknowledge exactly once, queue nothing, and warn:

```text
ferry: <relative-path>: save the file and retry
```

Pass guards through `Work` without claiming. Push/Compile need none.
Explicitly enabled pull-on-open gets a guard only for matching open content;
restored dirty content skips network work and warns. Default-disabled open is
silent.

- [ ] **Step 5: Verify and commit**

```sh
cargo test lsp::document_state::tests
cargo test capabilities_advertise_text_sync_and_exact_ferry_actions
cargo test automatic_
cargo test execute_command_
cargo test
git add src/lsp.rs
git commit -m "feat: enforce save-first Ferry actions"
```

### Task 7: Add Compare with Remote

**Files:**
- Modify: `src/lsp.rs:21-240,331-474,570-2006`
- Modify: `src/lsp/diff.rs`
- Modify: `src/bin/ferry-lsp.rs:1-24`

- [ ] **Step 1: Write failing action/comparison tests**

Add `COMPARE_COMMAND = "ferry.compare"`. Interim exact order:

```rust
[
    ("Ferry: Pull", PULL_COMMAND),
    ("Ferry: Compare with Remote", COMPARE_COMMAND),
    ("Ferry: Push", PUSH_COMMAND),
    ("Ferry: Compile-check", COMPILE_COMMAND),
]
```

With fake seams, test success argument order, retrieval/snapshot/launcher
failures, no Pull/state mutation, and blocked retrieval + didChange causing no
launch and one save-and-retry warning.

- [ ] **Step 2: Verify RED**

```sh
cargo test code_action_returns_exact_ferry_commands_for_project_file
cargo test compare_
```

- [ ] **Step 3: Construct a fallible production runtime**

Turn `FerryOperations` into a struct containing `SnapshotStore` and
`Box<dyn DiffLauncher>`:

```rust
impl FerryOperations {
    pub fn new() -> Result<Self> {
        Ok(Self {
            snapshots: SnapshotStore::new()?,
            launcher: Box::new(ZedDiffLauncher),
        })
    }
}
```

Update the binary to `Server::new(FerryOperations::new()?)`.

- [ ] **Step 4: Implement Compare on the worker**

Capture local identity, call `fetch_remote_one`, create snapshot, claim guard
immediately before launch, recapture/compare local identity, launch local first
and snapshot second, retain snapshot only after spawn. Map cancelled guards to
save-and-retry, not generic transport. Worker owns FTP/file/spawn work;
protocol remains responsive.

- [ ] **Step 5: Add command parsing and feedback**

Add `ActionCommand::Compare`, advertisement, one-file URI parsing, execution
re-resolution, and success message:

```text
ferry: <relative-path>: opened native diff
```

Safe errors never expose config/auth details.

- [ ] **Step 6: Verify the asynchronous edit race**

Block fake retrieval, send didChange, release, then assert immediate command
response, no launch/mutation, one warning, and prompt shutdown.

- [ ] **Step 7: Verify and commit**

```sh
cargo test compare_
cargo test code_action_returns_exact_ferry_commands_for_project_file
cargo test code_action_remains_responsive_while_transfer_worker_is_blocked
cargo test main_loop_shutdown_does_not_wait_for_blocked_file_operation
cargo test
git add src/lsp.rs src/lsp/diff.rs src/bin/ferry-lsp.rs
git commit -m "feat: compare Ferry files in Zed native diff"
```

### Task 8: Make normal Pull commit through the guard

**Files:**
- Modify: `src/lsp.rs:29-198,350-474,570-2006`

- [ ] **Step 1: Write failing two-phase tests**

Change fakes to record preparation/application separately. Test manual and
enabled-open preparation with false force, no-op revalidation, preparation
failure without apply, blocked preparation + didChange cancellation, and disk
identity changes rejecting apply.

- [ ] **Step 2: Verify RED**

```sh
cargo test pull_edit_during_preparation_cancels_commit
```

Expected: current indivisible `pull_one` cannot cancel before write.

- [ ] **Step 3: Split the operations seam**

```rust
fn prepare_pull(
    &mut self,
    config_path: &Path,
    rel: &str,
    force: bool,
) -> Result<PreparedPull>;

fn apply_pull(&mut self, prepared: PreparedPull) -> Result<TransferOutcome>;
```

Production delegates to `prepare_pull_one` and
`apply_prepared_pull(..., ExecutionMode::Apply)`.

- [ ] **Step 4: Claim at the final worker boundary**

Prepare without mutation, call `guard.try_claim()` immediately before apply,
drop/warn on failure, otherwise call core apply which independently checks disk
identity. Always claim/revalidate even for a no-op outcome so an edit during
remote preparation is reported as refusal.

- [ ] **Step 5: Verify and commit**

```sh
cargo test pull_edit_during_preparation_cancels_commit
cargo test execute_command_manual_pull_and_push_run_once_without_force
cargo test automatic_
cargo test queued_command_re_resolves_after_project_roots_change
cargo test main_loop_
cargo test
git add src/lsp.rs
git commit -m "fix: cancel pending Zed pulls after edits"
```

### Task 9: Add confirmed Force Pull correlation

**Files:**
- Modify: `src/lsp.rs:21-240,350-474,570-2006`

- [ ] **Step 1: Write failing final-action/confirmation tests**

Add `FORCE_PULL_COMMAND = "ferry.forcePull"`. Final exact action order:

```rust
[
    ("Ferry: Pull", PULL_COMMAND),
    ("Ferry: Compare with Remote", COMPARE_COMMAND),
    ("Ferry: Force Pull (overwrite local)", FORCE_PULL_COMMAND),
    ("Ferry: Push", PUSH_COMMAND),
    ("Ferry: Compile-check", COMPILE_COMMAND),
]
```

Test: prepare-before-response, exact warning request/actions, affirmative apply
once, Cancel/null/malformed/unknown no-op, didChange cancellation, saved-local
mismatch, same-file supersession, independent different files, and shutdown
dropping pending confirmations.

- [ ] **Step 2: Verify RED**

```sh
cargo test force_pull_
cargo test code_action_returns_exact_ferry_commands_for_project_file
```

- [ ] **Step 3: Add typed worker results**

```rust
enum WorkerEvent {
    Message(Message),
    ForcePullReady(PendingForcePull),
}

struct PendingForcePull {
    absolute_path: PathBuf,
    relative_path: String,
    prepared: PreparedPull,
    guard: OperationGuard,
}
```

Worker runs `prepare_force_pull_one`, never waits for Zed, and never writes
while preparing.

- [ ] **Step 4: Send/correlate native requests**

Coordinator owns a monotonic counter, `HashMap<RequestId, PendingForcePull>`,
and path-to-ID map. Use string IDs `ferry-force-pull-N`.
Build warning-level `ShowMessageRequestParams` with exact actions
`Overwrite local file` and `Cancel`, using
`ShowMessageRequest::METHOD`. Supersede/cancel older same-file pending data;
ignore its later response.

- [ ] **Step 5: Handle responses safely**

For a matching response, deserialize `Option<MessageActionItem>`; require
exact affirmative title, treat everything else as cancellation, re-resolve
project/file, and queue apply only while the guard is pending. Worker claims
immediately before `apply_prepared_pull`. Success uses transferred feedback;
guard/identity failure uses save-and-retry/conflict feedback. Unknown IDs remain
ignored.

- [ ] **Step 6: Preserve responsiveness**

Extend blocked-worker tests so Code Actions, command acknowledgements,
confirmation responses, and shutdown continue while FTP preparation is
blocked. Worker retains no LSP sender clone.

- [ ] **Step 7: Verify and commit**

```sh
cargo test force_pull_
cargo test code_action_returns_exact_ferry_commands_for_project_file
cargo test main_loop_
cargo test execute_command_
cargo test
git add src/lsp.rs
git commit -m "feat: confirm force pulls from Zed"
```

### Task 10: Document, verify, and hand off

**Files:**
- Modify: `README.md:140-181`
- Modify: `extensions/ferry/README.md:1-75`
- Verify: all files changed since `b6170cb`

- [ ] **Step 1: Capture failing documentation checks**

```sh
rg -n 'pull_on_open = false' README.md extensions/ferry/README.md
rg -n 'Ferry: Compare with Remote' README.md extensions/ferry/README.md
rg -n 'Ferry: Force Pull \(overwrite local\)' README.md extensions/ferry/README.md
```

Expected: new defaults/actions are not fully documented.

- [ ] **Step 2: Update both documents**

Cover: both defaults false; explicit per-project opt-in; five actions in order;
save-first Pull/Compare/Force; local-left/remote-right; Compare no mutation;
confirmed overwrite; `zed` CLI visibility; CLI force behavior unchanged.
Remove true-default and exactly-three-action statements.

- [ ] **Step 3: Re-run documentation checks**

Expected: both files contain all required statements.

- [ ] **Step 4: Run complete non-live verification**

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --manifest-path extensions/ferry/Cargo.toml
cargo check --manifest-path extensions/ferry/Cargo.toml --target wasm32-wasip2
git diff --check b6170cb..HEAD
```

Expected: zero exits; Docker cases remain ignored.

- [ ] **Step 5: Run controlled Docker regressions**

```sh
cargo test --test editor_sync_integration -- --ignored --test-threads=1
cargo test --test pull_integration -- --ignored --test-threads=1
```

Expected: controlled retrieval/force and existing CLI Pull cases pass.

- [ ] **Step 6: Audit credentials and scope**

```sh
git diff --stat b6170cb..HEAD
git diff --check b6170cb..HEAD
rg -n 'password|connection/authentication|run a Ferry task for details' src/lsp.rs src/lsp tests/editor_sync_integration.rs
```

Expected: only dummy test values; no real host/user/password/config text; no
3S config or generated WASM in the diff.

- [ ] **Step 7: Commit docs**

```sh
git add README.md extensions/ferry/README.md
git commit -m "docs: explain Zed remote diff and force pull"
```

- [ ] **Step 8: Request code review**

Invoke `@superpowers:requesting-code-review` for the full implementation
range. Fix Critical/Important findings test-first and rerun affected checks.

- [ ] **Step 9: Perform controlled Zed smoke test**

After review, request explicit GUI approval. Use a disposable Ferry project
backed by controlled FTP, never 3S. Verify five actions, diff orientation and
no mutation, unsaved blocking, cancel no-op, confirmed overwrite, and no
automatic operations with omitted editor settings. Stop/remove only the
explicit disposable server and temp path.

- [ ] **Step 10: Final verification and branch handoff**

Invoke `@superpowers:verification-before-completion`, rerun Steps 4-6, then
invoke `@superpowers:finishing-a-development-branch`.

Do not install from the feature worktree. After user-selected integration,
fast-forward durable `/home/simon/code/zed_ftp` main while preserving its
untracked `extensions/ferry/extension.wasm`, then install only from durable
main:

```sh
cargo install --path /home/simon/code/zed_ftp
```

Reload the development extension if necessary. Final live checks may inspect
the process/action labels but may not invoke a 3S network action.

## Acceptance checklist

- [ ] Both automatic editor settings default false; explicit project values remain authoritative.
- [ ] Zed advertises the five actions in approved order.
- [ ] Compare launches `zed --diff <local> <snapshot>` and changes no project/state data.
- [ ] Pull, Compare, and Force Pull reject initial dirtiness and edits during preparation.
- [ ] Force Pull applies only after exact affirmative response and local identity recheck.
- [ ] Cancel/dismissal/unknown response/shutdown/race/failure preserve local bytes and state.
- [ ] Snapshot roots are private, locked, safely cleaned, and outside the project.
- [ ] CLI force Pull, Push, Compile-check, and project resolution remain compatible.
- [ ] Non-live, Docker, formatting, clippy, extension, and review checks pass.
- [ ] No live credential material or generated extension WASM enters the commit range.
