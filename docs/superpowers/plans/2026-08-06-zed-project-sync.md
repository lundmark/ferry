# Project-Configurable Zed Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Ferry pull conflict-safely when Zed opens a file, optionally push when Zed saves it, and expose Pull, Push, and Compile-check as current-file Zed actions controlled by each project's `.ferry.toml`.

**Architecture:** Add a shared project/path resolver that maps absolute editor paths through the nearest Ferry configuration and its resolved `local_root`. Keep the Zed extension thin; move the language server into a testable library module that consumes structured, non-printing single-file sync and compile APIs. Zed project tasks call the same resolver by passing `$ZED_FILE`.

**Tech Stack:** Rust 2024, clap 4, anyhow/thiserror, serde/toml/serde_json, suppaftp, lsp-server 0.8, lsp-types 0.97, Zed extension API 0.6, tempfile, testcontainers.

**Approved design:** `docs/superpowers/specs/2026-08-06-zed-project-sync-design.md`

**Execution prerequisites:** Work in a dedicated Git worktree created with `@superpowers:using-git-worktrees`. Use `@superpowers:test-driven-development` for every behavior change. Never use the real 3S credentials or live server in automated tests.

---

## File Map

- Create `src/project.rs`: nearest-project discovery, legacy read-through/migration at both the config directory and resolved local root, canonical local-root containment, and absolute editor-path mapping.
- Create `src/commands/file_transfer.rs`: path-bearing structured outcomes shared by single-file Pull and Push.
- Create `src/lsp.rs`: server capabilities, event dispatch, Code Actions, command execution, feedback, and a fakeable Ferry-operation boundary.
- Create `tests/editor_sync_integration.rs`: ignored real-FTP tests for structured Pull/Push behavior; reuse `tests/support/mod.rs`.
- Modify `src/config.rs:5-68`: add project-local editor settings and backward-compatible defaults.
- Modify `src/lib.rs:1-11`: export the project and LSP modules.
- Modify `src/names.rs:18-43`: make legacy state migration file-aware and non-clobbering when both state directories exist.
- Modify `src/commands/mod.rs:21-75`: retain state-file read-through in apply mode when best-effort migration cannot complete.
- Modify `src/main.rs:61-100`: find existing configs upward for normal commands while keeping `init` rooted in the requested/current directory.
- Modify `src/commands/walk.rs:20-31`: accept absolute command arguments only after local-root containment.
- Modify `src/commands/pull.rs:21-339`: normalize arguments before connecting and return structured results from `pull_one`.
- Modify `src/commands/push.rs:21-214`: normalize arguments before connecting and add a non-printing `push_one`.
- Modify `src/commands/rm.rs`: route explicit paths through the shared safe argument resolver.
- Modify `src/commands/hook.rs:1-179`: use shared project/local-root resolution and adapt to the structured Pull result.
- Modify `src/commands/cc.rs:1-52`: return structured per-file compile results; remove `process::exit` from library code.
- Modify `src/commands/mod.rs:1-48`: export structured transfer types.
- Replace `src/bin/ferry-lsp.rs:1-210`: retain only stdio initialization and delegate to `ferry::lsp`.
- Modify `tests/cli_test.rs`: cover upward config lookup and safe absolute current-file arguments.
- Modify `examples/tasks.json`: use Ferry labels, `$ZED_FILE`, and `$ZED_DIRNAME`; add Compile-check.
- Modify `Cargo.toml`, `Cargo.lock`, `extensions/ferry/Cargo.toml`, `extensions/ferry/Cargo.lock`, and `extensions/ferry/extension.toml`: bump Ferry and the extension to 0.2.0.
- Modify `README.md` and `extensions/ferry/README.md`: document project settings, conflict behavior, Code Actions, tasks, installation, and the stale-content flash.
- Create `/home/simon/code/3s/.zed/tasks.json`: safe current-file actions and Status for this project.
- Modify `/home/simon/code/3s/.ferry.toml`: add only the approved `[editor]` booleans; never print or copy credential values.

### Task 1: Add Backward-Compatible Editor Settings

**Files:**
- Modify: `src/config.rs:5-68`

- [ ] **Step 1: Write failing configuration tests**

Add these tests beside the existing config tests:

```rust
#[test]
fn editor_defaults_preserve_pull_and_disable_push() {
    let cfg: Config = toml::from_str(
        r#"
        [connection]
        host = "h"
        user = "u"
        password = "p"
        [paths]
        remote_root = "/"
        "#,
    )
    .unwrap();

    assert!(cfg.editor.pull_on_open);
    assert!(!cfg.editor.push_on_save);
}

#[test]
fn parses_explicit_editor_settings() {
    let cfg: Config = toml::from_str(
        r#"
        [connection]
        host = "h"
        user = "u"
        password = "p"
        [paths]
        remote_root = "/"
        [editor]
        pull_on_open = false
        push_on_save = true
        "#,
    )
    .unwrap();

    assert_eq!(
        cfg.editor,
        Editor { pull_on_open: false, push_on_save: true }
    );
}
```

- [ ] **Step 2: Run the tests to verify RED**

Run: `cargo test config::tests::editor -- --nocapture`

Expected: FAIL because `Config` has no `editor` field or `Editor` type.

- [ ] **Step 3: Implement the minimal schema**

Add the field and type:

```rust
#[derive(Debug, Deserialize, PartialEq)]
pub struct Config {
    pub connection: Connection,
    pub paths: Paths,
    #[serde(default)]
    pub sync: Sync,
    #[serde(default)]
    pub editor: Editor,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Editor {
    #[serde(default = "default_true")]
    pub pull_on_open: bool,
    #[serde(default)]
    pub push_on_save: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self { pull_on_open: true, push_on_save: false }
    }
}
```

Do not change the existing `default_true`; both FTP passive mode and editor pull use it.

- [ ] **Step 4: Run focused and full tests**

Run:

```bash
cargo test config::tests -- --nocapture
cargo test
```

Expected: all tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add project editor sync settings"
```

### Task 2: Centralize Project Discovery and Local-Root Containment

**Files:**
- Create: `src/project.rs`
- Modify: `src/lib.rs:1-11`
- Modify: `src/names.rs:18-43`
- Modify: `src/commands/mod.rs:21-75`

- [ ] **Step 1: Write failing resolver tests**

Export `pub mod project;` from `src/lib.rs`, then create `src/project.rs` with
the public signatures below as `todo!()` stubs and a test module. This ensures
the RED command compiles and runs the new tests instead of filtering out an
unexported module. Cover:

```rust
#[test]
fn nearest_nested_config_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let outer = tmp.path();
    let inner = outer.join("nested");
    std::fs::create_dir_all(inner.join("mirror")).unwrap();
    write_config(outer, ".");
    write_config(&inner, "mirror");
    let file = inner.join("mirror/file.c");
    std::fs::write(&file, "").unwrap();

    let resolved = resolve_file(&file, false).unwrap().unwrap();
    assert_eq!(resolved.config_dir, inner);
    assert_eq!(resolved.relative_path, "file.c");
}

#[test]
fn maps_file_through_descendant_local_root() {
    let tmp = tempfile::tempdir().unwrap();
    let mirror = tmp.path().join("mirror/sub");
    std::fs::create_dir_all(&mirror).unwrap();
    write_config(tmp.path(), "mirror");
    let file = mirror.join("room.c");
    std::fs::write(&file, "").unwrap();

    let resolved = resolve_file(&file, false).unwrap().unwrap();
    assert_eq!(resolved.relative_path, "sub/room.c");
}

#[test]
fn rejects_file_outside_configured_local_root() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("mirror")).unwrap();
    write_config(tmp.path(), "mirror");
    let file = tmp.path().join("outside.c");
    std::fs::write(&file, "").unwrap();

    let error = resolve_file(&file, false).unwrap_err();
    assert!(error.to_string().contains("outside local_root"));
}
```

Also test legacy config discovery, no-config returning `Ok(None)`, a
non-existent new file with an existing parent, and Unix symlinks that resolve
outside `local_root`. Add this apply-mode migration regression:

```rust
#[test]
fn apply_mode_migrates_state_below_descendant_local_root() {
    let tmp = tempfile::tempdir().unwrap();
    let mirror = tmp.path().join("mirror");
    std::fs::create_dir_all(mirror.join(".ferry")).unwrap();
    std::fs::create_dir_all(mirror.join(".zed-ftp")).unwrap();
    write_config(tmp.path(), "mirror");
    let file = mirror.join("room.c");
    std::fs::write(&file, "").unwrap();
    std::fs::write(mirror.join(".zed-ftp/state.json"), r#"{"files":{}}"#).unwrap();

    resolve_file(&file, true).unwrap().unwrap();

    assert_eq!(
        std::fs::read_to_string(mirror.join(".ferry/state.json")).unwrap(),
        r#"{"files":{}}"#,
    );
    assert!(!mirror.join(".zed-ftp/state.json").exists());
}
```

In `names.rs`, add a second regression with both state files present and assert
that migration never overwrites `.ferry/state.json` or deletes the unmatched
legacy file. In `commands/mod.rs`, add an apply-mode test with an empty
`.ferry/` and a legacy state file; `state_path_for(..., Apply)` must select the
legacy file until migration succeeds.

- [ ] **Step 2: Run the new module tests to verify RED**

Run: `cargo test project::tests -- --nocapture`

Expected: FAIL in the `todo!()` resolver implementation; the test runner must
report the new project tests rather than zero matching tests.

- [ ] **Step 3: Implement the focused project module**

Use these public interfaces:

```rust
pub struct ProjectLocation {
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
}

pub struct ResolvedFile {
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
    pub config: Config,
    pub relative_path: String,
}

pub fn find_config_upward(start: &Path) -> Option<ProjectLocation>;
pub fn resolve_file(path: &Path, migrate_legacy: bool)
    -> anyhow::Result<Option<ResolvedFile>>;
pub fn relative_to_local_root(local_root: &Path, path: &Path)
    -> anyhow::Result<String>;
```

Implementation rules:

1. Begin at `start` when it is a directory, otherwise at its parent.
2. Prefer `.ferry.toml` over `.zed-ftp.toml` in each directory.
3. Stop at the first matching ancestor.
4. In `resolve_file`, when `migrate_legacy` is true, migrate the discovered
   config directory and call `config_path_for_read` again before loading config.
5. After loading config, migrate `config.paths.local_root` as a second,
   idempotent, best-effort step when `migrate_legacy` is true. This preserves
   legacy state stored under a descendant local root such as
   `mirror/.zed-ftp/state.json`. A migration warning must not prevent resolving
   through legacy config/state paths.
6. Refine `names::migrate_legacy`: when `.ferry/` is absent, the existing
   whole-directory rename remains valid; when `.ferry/` already exists and
   `.ferry/state.json` does not, move `.zed-ftp/state.json` into it at file
   granularity, then remove the legacy directory only if empty. If the current
   state file already exists, never overwrite it and leave the legacy file
   untouched. Keep contextual errors so callers can report migration warnings.
7. Make `state_path_for` prefer the current state *file*, then fall back to the
   legacy state file in both Apply and DryRun modes. Normally apply-mode
   migration moves the file first; this fallback preserves conflict/cooldown
   history when migration is blocked and may keep writing the legacy file
   until a later migration succeeds.
8. When `migrate_legacy` is false, do not mutate either location. Retain
   legacy read-through through `config_path_for_read` and `state_path_for`.
9. Canonicalize `local_root`. Canonicalize an existing file; for a new file,
   canonicalize its existing parent and append its file name.
10. Require `candidate.strip_prefix(canonical_local_root)` to succeed.
11. Convert separators to `/` and pass the result through `walk::safe_rel`.

Export `pub mod project;` from `src/lib.rs`. Keep `names::config_path_for_read` and migration as the single source of legacy filenames.

- [ ] **Step 4: Run resolver tests**

Run:

```bash
cargo test project::tests -- --nocapture
cargo test names::tests -- --nocapture
cargo test commands::execution_mode_tests -- --nocapture
```

Expected: PASS, including nested and containment cases.

- [ ] **Step 5: Commit**

```bash
git add src/project.rs src/lib.rs src/names.rs src/commands/mod.rs
git commit -m "feat: resolve editor files within Ferry projects"
```

### Task 3: Use the Resolver in CLI Paths and the Agent Hook

**Files:**
- Modify: `src/main.rs:61-100`
- Modify: `src/commands/walk.rs:20-31`
- Modify: `src/commands/pull.rs:21-102`
- Modify: `src/commands/push.rs:21-66`
- Modify: `src/commands/rm.rs`
- Modify: `src/commands/cc.rs:12-25`
- Modify: `src/commands/hook.rs:48-179`
- Modify: `tests/cli_test.rs`

- [ ] **Step 1: Write failing path and config-discovery tests**

Add unit tests for:

```rust
#[test]
fn absolute_arg_inside_root_becomes_relative() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("sub/file.c");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "").unwrap();
    assert_eq!(
        safe_arg(tmp.path(), file.to_str().unwrap()).unwrap(),
        "sub/file.c"
    );
}

#[test]
fn absolute_arg_outside_root_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let file = other.path().join("file.c");
    std::fs::write(&file, "").unwrap();
    assert!(safe_arg(root.path(), file.to_str().unwrap()).is_err());
}
```

In `tests/cli_test.rs`, add a test that runs `ferry status` from a nested directory with a config in an ancestor and asserts stderr reaches the configured `127.0.0.1:1` connection rather than reporting a missing config. Add a pure resolver test confirming `init` still selects `cwd/.ferry.toml`.

Add an apply-mode hook regression with a config at the project root and
`local_root = "mirror"`. Put a recent matching state entry at
`mirror/.zed-ftp/state.json`, pre-create an empty `mirror/.ferry/`, invoke
`ferry hook --cooldown 3600` with the target file in the hook JSON, and assert:

- the hook exits successfully;
- stderr reports the cooldown skip and never an FTP connection attempt;
- `mirror/.ferry/state.json` exists; and
- `mirror/.zed-ftp/state.json` no longer exists.

Add a best-effort failure variant that blocks migration by making
`mirror/.ferry` a regular file while leaving the recent legacy state intact.
The hook must still exit successfully, read the legacy cooldown record, report
the cooldown skip without attempting FTP, and leave the legacy state unchanged.

- [ ] **Step 2: Run tests to verify RED**

Run:

```bash
cargo test absolute_arg -- --nocapture
cargo test --test cli_test finds_config_upward -- --nocapture
```

Expected: FAIL because absolute arguments are refused and the CLI reads only `./.ferry.toml`.

- [ ] **Step 3: Add safe absolute argument mapping**

In `walk.rs`:

```rust
pub fn safe_arg(local_root: &Path, input: &str) -> Result<String> {
    let path = Path::new(input);
    if path.is_absolute() {
        return crate::project::relative_to_local_root(local_root, path);
    }
    safe_rel(input)
}
```

Load config and normalize all explicit arguments before connecting to FTP in Pull, Push, and Remove. Use `safe_arg` in Compile-check too. Preserve bare-command behavior and existing relative argument semantics.

- [ ] **Step 4: Make normal CLI lookup walk upward without changing init**

Extract a helper used by `main::run`:

```rust
fn default_config_path(cmd: &Cmd) -> PathBuf {
    if matches!(cmd, Cmd::Init { .. }) {
        return PathBuf::from(ferry::names::CONFIG_FILE);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    ferry::project::find_config_upward(&cwd)
        .map(|location| location.config_path)
        .unwrap_or_else(|| ferry::names::config_path_for_read(&cwd))
}
```

For apply mode, migrate the discovered config directory before finalizing the
path and, after loading `Config`, migrate state beneath the resolved
`config.paths.local_root`. Do both steps for an explicit `--config` as well,
while keeping that path authoritative. Treat migration errors as warnings and
continue through the legacy read paths; never discard state history merely
because the rename could not complete. For dry-run, retain config and state
read-through without mutation. `ferry init` must continue creating
configuration in its current/requested directory rather than adopting an
ancestor project.

- [ ] **Step 5: Refactor the hook to shared resolution**

Replace `find_project_dir_upward`, `existing_or`, and root-relative stripping with `project::resolve_file(file_path, mode.should_apply())`. Derive state from `resolved.config.paths.local_root` in both modes. For the cooldown read, call the shared file-based `state_path_for`; it must fall back to legacy state after a best-effort migration failure. Log resolution/migration failures and return `Ok(())` so malformed or read-only projects never block the invoking agent. Preserve:

- no-op outside Ferry projects;
- cooldown behavior;
- best-effort legacy migration;
- force=true for the pre-existing agent-hook contract; and
- always-successful hook exit behavior.
Keep the current boolean `pull_one` match in this task; Task 4 updates that
match in the same commit that changes the return type.

- [ ] **Step 6: Run focused and regression tests**

Run:

```bash
cargo test commands::walk -- --nocapture
cargo test commands::hook -- --nocapture
cargo test --test cli_test hook_migrates_descendant_local_root_state -- --nocapture
cargo test --test cli_test hook_reads_legacy_state_when_migration_fails -- --nocapture
cargo test --test cli_test -- --nocapture
cargo test
```

Expected: all PASS. Unsafe paths fail before any network attempt.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs src/commands/walk.rs src/commands/pull.rs src/commands/push.rs src/commands/rm.rs src/commands/cc.rs src/commands/hook.rs tests/cli_test.rs
git commit -m "feat: discover Ferry projects for editor paths"
```

### Task 4: Add Structured, Non-Printing Single-File Transfers

**Files:**
- Create: `src/commands/file_transfer.rs`
- Create: `tests/editor_sync_integration.rs`
- Modify: `src/commands/mod.rs:1-48`
- Modify: `src/commands/pull.rs:265-339`
- Modify: `src/commands/push.rs:1-214`
- Modify: `src/commands/hook.rs`

- [ ] **Step 1: Define outcome tests and Docker-gated behavior tests**

Use this result model:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    Unchanged,
    Transferred,
    SkippedMissingSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub path: String,
    pub status: TransferStatus,
}
```

In `tests/editor_sync_integration.rs`, reuse `mod support;` and write ignored tests proving:

- `pull_one(..., false, Apply)` returns `Transferred` and writes remote bytes locally;
- a local conflict returns `Exit::Conflict` whose display contains the relative path and preserves the local file;
- `push_one(..., false, Apply)` returns `Transferred` and creates/updates the remote file;
- a remote conflict contains the path and preserves the remote file; and
- generic FTP/transport errors from both `pull_one` and `push_one` include the
  local-root-relative path in their error chains.

- [ ] **Step 2: Compile tests to verify RED**

Run: `cargo test --test editor_sync_integration --no-run`

Expected: FAIL because `TransferOutcome` and `push_one` do not exist.

- [ ] **Step 3: Convert Pull's single-file API**

Change the signature to:

```rust
pub fn pull_one(
    config_path: &Path,
    rel: &str,
    force: bool,
    mode: ExecutionMode,
) -> Result<TransferOutcome>
```

Map states exactly:

- `InSync` -> `Unchanged`;
- `LocalOnly` -> `SkippedMissingSource`;
- `RemoteOnly | RemoteChanged` -> download and `Transferred`;
- conflict states -> existing path-specific `Exit::Conflict`, unless the pre-existing hook passes force.

Do not print inside `pull_one`. Wrap the entire single-file operation with
path context (for example, `with_context(|| format!("pull {rel}"))`) so every
error, not only conflicts, identifies the relative path.

- [ ] **Step 4: Implement Push's matching single-file API**

Add:

```rust
pub fn push_one(
    config_path: &Path,
    rel: &str,
    force: bool,
    mode: ExecutionMode,
) -> Result<TransferOutcome>
```

Use the same state path, FTP connection, local-byte hashing, remote hashing, classification, atomic upload, and state-save helpers as aggregate Push. Map:

- `InSync` -> `Unchanged`;
- `RemoteOnly` -> `SkippedMissingSource`;
- `LocalOnly | LocalChanged` -> upload and `Transferred`;
- `RemoteChanged | BothChanged | Untracked` -> path-specific conflict unless force.

Neither single-file API may call `println!`, `eprintln!`, or `process::exit`.
Wrap the whole Push operation with equivalent `push {rel}` context so every
returned error identifies the relative path.

- [ ] **Step 5: Update the hook and tests**

Translate outcomes:

```rust
match pull_one(&resolved.config_path, &resolved.relative_path, true, mode) {
    Ok(outcome) if outcome.status == TransferStatus::Transferred => {
        eprintln!("ferry hook: pulled {}", outcome.path);
    }
    Ok(_) => {}
    Err(error) => eprintln!(
        "ferry hook: pull {} failed: {error:#}",
        resolved.relative_path
    ),
}
```

Retain `would pull` wording in dry-run mode.

- [ ] **Step 6: Run tests**

Run:

```bash
cargo test
cargo test --test editor_sync_integration --no-run
```

If Docker is available, also run:

```bash
cargo test --test editor_sync_integration -- --ignored --nocapture --test-threads=1
```

Expected: unit suite PASS; integration test compiles, and runtime suite PASS when Docker is available.

- [ ] **Step 7: Commit**

```bash
git add src/commands/file_transfer.rs src/commands/mod.rs src/commands/pull.rs src/commands/push.rs src/commands/hook.rs tests/editor_sync_integration.rs
git commit -m "feat: add structured single-file sync operations"
```

### Task 5: Make Compile-Check Reusable by the LSP

**Files:**
- Modify: `src/commands/cc.rs:1-52`
- Modify: `src/main.rs:81-100`

- [ ] **Step 1: Write failing structured-result tests**

Define a fake checker and assert a mixed pass/fail/error set retains each path and diagnostics:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCheckStatus {
    Passed,
    Failed,
    TransportError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheckResult {
    pub path: String,
    pub status: FileCheckStatus,
    pub diagnostics: String,
}
```

Add a private `CompileTransport` trait implemented by `CompileClient`, plus tests for `check_with` using a fake transport. Assert the result path is always local-root-relative.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test commands::cc::tests -- --nocapture`

Expected: FAIL because structured result APIs do not exist.

- [ ] **Step 3: Implement collection separately from CLI formatting**

Public API:

```rust
pub fn check_files(
    config_path: &Path,
    paths: &[String],
) -> Result<Vec<FileCheckResult>>;

pub fn run(config_path: &Path, paths: &[String]) -> Result<()>;
```

`check_files` loads config, safely resolves each input, invokes the transport, and returns data without printing or exiting. `run` formats the returned results exactly once and returns `Err(anyhow!("one or more compile checks failed"))` if any status is not `Passed`. Let `main` map that ordinary error to exit 1; remove `std::process::exit` from command code.

- [ ] **Step 4: Run compile and CLI regressions**

Run:

```bash
cargo test commands::cc -- --nocapture
cargo test --test cli_test -- --nocapture
cargo test
```

Expected: PASS. Existing `ferry cc` output remains per-file and exits non-zero on compile failure.

- [ ] **Step 5: Commit**

```bash
git add src/commands/cc.rs src/main.rs
git commit -m "refactor: return structured compile results"
```

### Task 6: Implement Configurable Pull-on-Open and Push-on-Save

**Files:**
- Create: `src/lsp.rs`
- Modify: `src/lib.rs:1-11`
- Replace: `src/bin/ferry-lsp.rs:1-210`

- [ ] **Step 1: Write failing LSP event tests**

Export `pub mod lsp;` from `src/lib.rs`, then create `src/lsp.rs` with the
fakeable boundary and `todo!()` handler stubs below. This makes the RED command
compile and execute the new tests instead of matching no module:

```rust
pub trait FileOperations {
    fn pull(
        &mut self,
        config_path: &Path,
        rel: &str,
        force: bool,
    ) -> anyhow::Result<TransferOutcome>;

    fn push(
        &mut self,
        config_path: &Path,
        rel: &str,
        force: bool,
    ) -> anyhow::Result<TransferOutcome>;

    fn compile(
        &mut self,
        config_path: &Path,
        rel: &str,
    ) -> anyhow::Result<FileCheckResult>;
}
```

Using temporary project configs and `FakeOperations`, test:

- open with default settings calls Pull exactly once with `force=false`;
- `pull_on_open=false` calls nothing;
- save with default settings calls nothing;
- `push_on_save=true` calls Push exactly once with `force=false`;
- non-file URIs and paths outside Ferry projects call nothing;
- conflicts and generic failures emit warning notifications containing the relative path;
- successful automatic operations emit no notification.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test lsp::tests::automatic -- --nocapture`

Expected: FAIL in the stub event dispatcher; the runner must report the new
automatic-event tests rather than zero matching tests.

- [ ] **Step 3: Implement capabilities and event dispatch**

Advertise:

```rust
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
```

Create `Server<O: FileOperations>` with handlers for `textDocument/didOpen` and `textDocument/didSave`. Resolve the URI through `project::resolve_file(path, true)`, read `resolved.config.editor` on every event so setting changes require no LSP restart, and call the operation with `force=false`.

Use `window/showMessage` type Warning for conflicts and other failures. Do not report success for automatic operations.

- [ ] **Step 4: Make the binary a thin transport adapter**

`src/bin/ferry-lsp.rs` should only:

1. open `Connection::stdio()`;
2. initialize with `ferry::lsp::capabilities()`;
3. call `ferry::lsp::main_loop(connection, Server::new(FerryOperations))`;
4. join I/O threads.

Move URI parsing and existing tests into `src/lsp.rs`. Ensure no library operation used by `FerryOperations` prints to stdout.

- [ ] **Step 5: Run LSP and full tests**

Run:

```bash
cargo test lsp::tests -- --nocapture
cargo test --bin ferry-lsp -- --nocapture
cargo test
```

Expected: PASS. The automatic-event tests explicitly record `force=false`.

- [ ] **Step 6: Commit**

```bash
git add src/lsp.rs src/lib.rs src/bin/ferry-lsp.rs
git commit -m "feat: sync Zed files on open and configured save"
```

### Task 7: Add Pull, Push, and Compile-check Code Actions

**Files:**
- Modify: `src/lsp.rs`

- [ ] **Step 1: Write failing Code Action and command tests**

Add tests asserting a valid Ferry file returns exactly:

```text
Ferry: Pull          -> ferry.pull
Ferry: Push          -> ferry.push
Ferry: Compile-check -> ferry.compile
```

Each command receives one URI string argument. Also test:

- no actions outside a Ferry project;
- malformed/missing arguments produce a JSON-RPC error response without executing;
- manual Pull/Push use `force=false`;
- transferred/unchanged/skipped manual outcomes show concise Info messages;
- conflicts and failures show Warning messages with the path;
- compile pass shows Info;
- compile failure shows Warning including diagnostics.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test lsp::tests::code_action -- --nocapture`

Expected: FAIL because request dispatch and commands are absent.

- [ ] **Step 3: Implement request dispatch**

Define:

```rust
pub const PULL_COMMAND: &str = "ferry.pull";
pub const PUSH_COMMAND: &str = "ferry.push";
pub const COMPILE_COMMAND: &str = "ferry.compile";
pub const ACTION_COMMANDS: [&str; 3] =
    [PULL_COMMAND, PUSH_COMMAND, COMPILE_COMMAND];
```

Extend `capabilities()` in this task with
`CodeActionProviderCapability::Simple(true)` and an
`ExecuteCommandOptions` containing `ACTION_COMMANDS`. Task 6 deliberately
advertises only text synchronization so it remains independently compiling.

For `textDocument/codeAction`, parse `CodeActionParams`, resolve the URI without performing network I/O, and return three `CodeActionOrCommand::Command` entries:

```rust
Command::new(
    "Ferry: Pull".into(),
    PULL_COMMAND.into(),
    Some(vec![serde_json::Value::String(uri.to_string())]),
)
```

For `workspace/executeCommand`, parse `ExecuteCommandParams`, validate exactly one file URI, resolve it again, run the selected structured API, send feedback, and return a successful JSON-RPC response. Unknown commands and malformed arguments return `ErrorCode::InvalidParams`.

- [ ] **Step 4: Add an in-memory protocol regression**

Use `Connection::memory()` to send a Code Action request and shutdown through
`main_loop`. Assert every server output is a valid
`lsp_server::Message` response/notification and that the fake operation sees
no force. This locks in the rule that the server communicates through LSP
messages rather than stdout text.

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test lsp::tests -- --nocapture
cargo test
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/lsp.rs
git commit -m "feat: expose Ferry actions in Zed"
```

### Task 8: Update Tasks, Documentation, and Release Metadata

**Files:**
- Modify: `examples/tasks.json`
- Modify: `README.md`
- Modify: `extensions/ferry/README.md`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `extensions/ferry/Cargo.toml`
- Modify: `extensions/ferry/Cargo.lock`
- Modify: `extensions/ferry/extension.toml`

- [ ] **Step 1: Update the example Zed tasks**

Current-file tasks use this exact shape:

```json
{
  "label": "Ferry: push current file",
  "command": "ferry",
  "args": ["push", "$ZED_FILE"],
  "cwd": "$ZED_DIRNAME",
  "use_new_terminal": false,
  "reveal": "on_error"
}
```

Provide Pull, Push, and Compile-check variants. Keep Status and project-wide Sync clearly labeled, with `cwd` set to `$ZED_WORKTREE_ROOT`. If Delete remains, label it `Ferry: DELETE current file locally and remotely` and keep the README warning.

- [ ] **Step 2: Validate task JSON**

Run: `python -m json.tool examples/tasks.json >/dev/null`

Expected: exit 0.

- [ ] **Step 3: Document configuration and behavior**

Document:

```toml
[editor]
pull_on_open = true
push_on_save = false
```

State both defaults, nearest-project behavior, non-forced conflict handling, the possible stale-content flash on open, silent automatic success, Code Actions via lightbulb/`Ctrl-.`, Task Picker alternatives, and that changing booleans is read on the next event.

Remove the extension README's claim that open always uses `--force`.

- [ ] **Step 4: Bump both packages to 0.2.0**

Update the root crate, extension crate, and `extension.toml`. Update the extension description to mention configurable pull-on-open and push-on-save. Refresh lockfiles through Cargo commands; do not edit lockfiles manually.

- [ ] **Step 5: Run documentation-adjacent build checks**

Run:

```bash
cargo check
cargo check --manifest-path extensions/ferry/Cargo.toml
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Expected: all commands exit 0 and report Ferry 0.2.0 where package versions are shown.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock examples/tasks.json README.md extensions/ferry/Cargo.toml extensions/ferry/Cargo.lock extensions/ferry/extension.toml extensions/ferry/README.md
git commit -m "docs: release configurable Zed integration"
```

### Task 9: Install and Configure the 3S Project Safely

**Files:**
- Modify: `/home/simon/code/3s/.ferry.toml`
- Create: `/home/simon/code/3s/.zed/tasks.json`

- [ ] **Step 1: Verify source and project state**

Run:

```bash
git status --short
git log -1 --oneline
```

Expected in Ferry: clean worktree at the final implementation commit. The 3S directory is intentionally not a Git repository.

- [ ] **Step 2: Add only the approved project settings**

Append without printing existing config values:

```toml
[editor]
pull_on_open = true
push_on_save = false
```

If `[editor]` already exists, update only these two keys. Do not copy credentials into patches, logs, test fixtures, or summaries.

- [ ] **Step 3: Add safe 3S project tasks**

Create `.zed/tasks.json` with:

- Ferry: pull current file
- Ferry: push current file
- Ferry: compile-check current file
- Ferry: status

Use `$ZED_FILE`/`$ZED_DIRNAME` for current-file tasks and `$ZED_WORKTREE_ROOT` for Status. Do not add whole-tree Sync or Delete to 3S because its live mirror is enormous and deletion is destructive.

- [ ] **Step 4: Validate local configuration without exposing it**

Run:

```bash
python -m json.tool /home/simon/code/3s/.zed/tasks.json >/dev/null
awk -F= '/^\[editor\]/{in_editor=1; print; next} /^\[/{in_editor=0} in_editor && /^[[:space:]]*(pull_on_open|push_on_save)[[:space:]]*=/{print}' /home/simon/code/3s/.ferry.toml
```

Expected:

```text
[editor]
pull_on_open = true
push_on_save = false
```

No connection fields or credential values may appear.

- [ ] **Step 5: Install updated binaries**

Run:

```bash
cargo install --path .
ferry --version
```

Run this from the implementation worktree. Expected: `ferry 0.2.0`.

- [ ] **Step 6: Install or reload the development extension**

If the `zed` CLI has been installed from Zed's application menu:

```bash
zed --dev-extension "$PWD/extensions/ferry"
```

Otherwise, in Zed run `Extensions: Install Dev Extension` and select:

```text
<the implementation worktree>/extensions/ferry
```

Expected: Ferry 0.2.0 appears as an installed development extension and `ferry-lsp` starts for C/header files.

- [ ] **Step 7: Perform non-destructive smoke checks**

In Zed:

1. Open a known in-sync, non-sensitive `.c` file and confirm no warning.
2. Trigger Code Actions and confirm Pull, Push, and Compile-check appear.
3. Save without changing content and confirm no remote push occurs because `push_on_save=false`.
4. Run Ferry: status from the Task Picker.
5. Do not test automatic push against the live 3S tree. If desired, create a temporary Ferry project backed by the existing Docker fixture, set `push_on_save=true` there, and verify save upload.

- [ ] **Step 8: Run final source verification**

From the Ferry repository:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --test editor_sync_integration --no-run
cargo check --manifest-path extensions/ferry/Cargo.toml
git status --short
```

Expected: all checks exit 0 and the Ferry worktree is clean. Run the ignored Docker editor-sync test only when Docker is available; do not claim it passed otherwise.

## Acceptance Criteria

- Existing projects without `[editor]` pull on open and never push on save.
- `pull_on_open=false` disables automatic pulls for that project.
- `push_on_save=true` enables non-forced single-file pushes for that project.
- Open and save conflicts preserve both sides and produce path-specific Zed warnings.
- Automatic success is silent; manual Code Actions provide concise feedback.
- Pull, Push, and Compile-check appear for the active Ferry file in Zed.
- Current-file tasks work when `local_root != "."` and when the Ferry project is nested inside a larger Zed worktree.
- Apply-mode discovery migrates legacy state beneath a descendant `local_root` even when an empty `.ferry/` already exists, never clobbers a current state file, and reads through legacy state after a best-effort migration failure; dry-run discovery only reads through legacy paths.
- No LSP-called operation prints to stdout or exits the process.
- The existing agent hook retains its established force/cooldown behavior.
- The 3S project is configured with safe defaults and no whole-tree/destructive tasks.
