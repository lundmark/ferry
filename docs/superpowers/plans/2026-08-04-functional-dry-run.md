# Functional Dry-Run Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Ferry's global `--dry-run` flag a reliable no-write preview across every command while preserving normal execution behavior.

**Architecture:** Introduce a shared `ExecutionMode` enum and pass it from CLI parsing to command and mutation boundaries. Commands continue normal discovery, hashing, validation, and conflict handling, but `DryRun` skips transfer, deletion, persistence, initialization, and migration writes while emitting explicit `would ...` output. A dedicated real-FTP integration fixture exercises the reported single-file push regression and the remaining mutating commands.

**Tech Stack:** Rust 2024, clap, anyhow, suppaftp, serde/serde_json, testcontainers 0.20, tempfile.

**Approved design:** `docs/superpowers/specs/2026-08-04-dry-run-execution-design.md`

---

## File Map

- Create `tests/support/mod.rs`: reusable Docker FTP fixture with a working passive-data port and `/home/test` remote root.
- Create `tests/dry_run_integration.rs`: end-to-end non-mutation tests for push, pull, sync, rm, hook, init validation, and status.
- Modify `src/commands/mod.rs`: define `ExecutionMode` and the dry-run-aware state-path selector.
- Modify `src/main.rs`: translate the flag once, suppress dry-run migration, select legacy config read-through, and pass execution mode.
- Modify `src/names.rs`: add read-only current/legacy config selection.
- Modify `src/commands/push.rs`: preview uploads and skip upload/state writes.
- Modify `src/commands/pull.rs`: preview downloads, including `pull_one`, and skip local/state writes.
- Modify `src/commands/sync.rs`: preview both transfer directions and skip all writes.
- Modify `src/commands/rm.rs`: preview file/directory deletions and skip local, remote, and state writes.
- Modify `src/commands/init.rs`: preview config, gitignore, state, and conflict-resolution actions.
- Modify `src/commands/hook.rs`: preview auto-pulls and suppress legacy migration.
- Modify `src/commands/status.rs`: suppress state-cache persistence in dry-run mode.
- Modify `src/bin/ferry-lsp.rs`: explicitly retain apply behavior after the pull API gains execution mode.
- Modify `tests/init_integration.rs`: add a non-Docker dry-run init regression.
- Modify `tests/cli_test.rs`: cover legacy migration suppression and legacy config read-through.
- Modify `README.md`: document the no-write guarantee and preview wording.

### Task 1: Establish the FTP Fixture and Fix `push <file> --dry-run`

**Files:**
- Create: `tests/support/mod.rs`
- Create: `tests/dry_run_integration.rs`
- Modify: `src/commands/mod.rs`
- Modify: `src/main.rs:5-85`
- Modify: `src/commands/push.rs:17-187`
- Modify: `src/commands/sync.rs:17-151`
- Modify: `src/commands/init.rs:14-275`

- [ ] **Step 1: Add a working real-FTP test fixture**

Create `tests/support/mod.rs`. The existing fixtures wait for a log line the image never emits and do not map the advertised passive port. Use one dynamically selected host port as both the container passive port and host mapping:

```rust
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use ferry::ftp::Ftp;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

pub const REMOTE_ROOT: &str = "/home/test";

pub struct FtpFixture {
    pub host: String,
    pub control_port: u16,
    pub container: Container<GenericImage>,
}

pub fn start_ftp() -> FtpFixture {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let passive_port = listener.local_addr().unwrap().port();
    drop(listener);

    let passive = passive_port.to_string();
    let image = GenericImage::new("delfer/alpine-ftp-server", "latest")
        .with_exposed_port(21.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("USERS", "test|testpw|/home/test")
        .with_env_var("ADDRESS", "127.0.0.1")
        .with_env_var("MIN_PORT", passive.clone())
        .with_env_var("MAX_PORT", passive)
        .with_mapped_port(passive_port, passive_port.tcp());
    let container = image.start().unwrap();
    let control_port = container.get_host_port_ipv4(21.tcp()).unwrap();
    let host = "127.0.0.1".to_string();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Ftp::connect(&host, control_port, "test", "testpw", true) {
            Ok(_) => break,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("FTP fixture did not become ready: {error:#}"),
        }
    }

    FtpFixture { host, control_port, container }
}

pub fn remote_path(rel: &str) -> String {
    format!("{REMOTE_ROOT}/{}", rel.trim_start_matches('/'))
}

pub fn write_config(
    local_root: &std::path::Path,
    fixture: &FtpFixture,
) -> std::path::PathBuf {
    let path = local_root.join(".ferry.toml");
    std::fs::write(
        &path,
        format!(
            "[connection]\nhost = {:?}\nport = {}\nuser = \"test\"\npassword = \"testpw\"\npassive = true\n\n[paths]\nlocal_root = {:?}\nremote_root = {:?}\n",
            fixture.host,
            fixture.control_port,
            local_root.display().to_string(),
            REMOTE_ROOT,
        ),
    )
    .unwrap();
    path
}
```

Keep the fixture private to integration tests. Do not rewrite the unrelated pre-existing Docker suites in this task.

- [ ] **Step 2: Write the failing single-file push regression**

In `tests/dry_run_integration.rs`, add `mod support;` and an ignored Docker test. Create a local-only `new.txt`, run `ferry push new.txt --dry-run` with an explicit config, then assert:

```rust
#[test]
#[ignore = "requires Docker"]
fn push_file_dry_run_does_not_upload_or_write_state() {
    let fixture = support::start_ftp();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("new.txt"), b"local only\n").unwrap();
    let config = support::write_config(dir.path(), &fixture);

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&config)
        .args(["push", "new.txt", "--dry-run"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr={}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("would push new.txt"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );

    let mut ftp = Ftp::connect(
        &fixture.host,
        fixture.control_port,
        "test",
        "testpw",
        true,
    )
    .unwrap();
    assert!(ftp.size(&support::remote_path("new.txt")).is_err());
    assert!(!dir.path().join(".ferry/state.json").exists());
}
```

Bind the returned fixture to `_fixture_guard` or otherwise keep its `container` field alive through every assertion.

- [ ] **Step 3: Run the push test and verify RED**

Run:

```bash
cargo test --test dry_run_integration push_file_dry_run_does_not_upload_or_write_state -- --ignored --nocapture
```

Expected: FAIL because the current CLI uploads `new.txt`, creates `.ferry/state.json`, and prints `pushed new.txt`.

- [ ] **Step 4: Add the shared execution mode**

In `src/commands/mod.rs`, add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Apply,
    DryRun,
}

impl ExecutionMode {
    pub fn from_dry_run(dry_run: bool) -> Self {
        if dry_run { Self::DryRun } else { Self::Apply }
    }

    pub fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }

    pub fn should_apply(self) -> bool {
        matches!(self, Self::Apply)
    }
}

#[cfg(test)]
mod execution_mode_tests {
    use super::ExecutionMode;

    #[test]
    fn maps_cli_flag_and_reports_write_permission() {
        assert_eq!(ExecutionMode::from_dry_run(false), ExecutionMode::Apply);
        assert_eq!(ExecutionMode::from_dry_run(true), ExecutionMode::DryRun);
        assert!(ExecutionMode::Apply.should_apply());
        assert!(!ExecutionMode::DryRun.should_apply());
    }
}
```

- [ ] **Step 5: Thread the mode through main and push**

In `src/main.rs`, construct the mode immediately after parsing:

```rust
let mode = ferry::commands::ExecutionMode::from_dry_run(cli.dry_run);
```

Pass `mode` to `push::run`. Also guard the default-path migration now so even the first push fix honors the global no-write contract:

```rust
if !explicit_config && mode.should_apply() {
    if let Err(e) = ferry::names::migrate_legacy(std::path::Path::new(".")) {
        eprintln!("warning: {e:#}");
    }
}
```

Change `push::run` to accept `mode: ExecutionMode`. Change `upload_one` to accept the same mode and return before `upload_remote_atomic` when dry-running:

```rust
pub fn upload_one(
    ftp: &mut Ftp,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    bytes: &[u8],
    new_hash: &str,
    mode: ExecutionMode,
) -> Result<()> {
    if mode.is_dry_run() {
        return Ok(());
    }
    upload_remote_atomic(ftp, remote_path, bytes)?;
    update_state_after_push(state, rel, ftp, remote_path, new_hash, bytes.len() as u64)
}
```

Pass `mode` at both push upload sites. Pass `ExecutionMode::Apply` from the not-yet-converted `sync` and `init` call sites so normal behavior continues compiling. Use:

```rust
println!("{} {rel}", if mode.is_dry_run() { "would push" } else { "pushed" });
```

For the forced branch, keep the current apply-mode message and emit `would overwrite remote with local (--force): <path>` in dry-run mode. Only call `state.save` when `mode.should_apply()`.

- [ ] **Step 6: Run focused tests and verify GREEN**

Run:

```bash
cargo test execution_mode_tests
cargo test --test dry_run_integration push_file_dry_run_does_not_upload_or_write_state -- --ignored --nocapture
cargo test --test push_integration --no-run
```

Expected: all PASS/compile; the regression test leaves remote and state untouched.

- [ ] **Step 7: Commit the push slice**

```bash
git add src/main.rs src/commands/mod.rs src/commands/push.rs src/commands/sync.rs src/commands/init.rs tests/support/mod.rs tests/dry_run_integration.rs
git commit -m "fix: make push dry-run non-mutating"
```

### Task 2: Make Pull and Hook Dry-Run Safe

**Files:**
- Modify: `tests/dry_run_integration.rs`
- Modify: `src/main.rs:75-85`
- Modify: `src/commands/pull.rs:15-370`
- Modify: `src/commands/hook.rs:25-125`
- Modify: `src/commands/sync.rs:15-135`
- Modify: `src/commands/init.rs:14-280`
- Modify: `src/bin/ferry-lsp.rs:80-95`

- [ ] **Step 1: Write failing pull and hook tests**

Add three ignored tests using the shared fixture:

1. `pull_file_dry_run_does_not_create_local_file_or_state` uploads a remote-only file, runs `pull <file> --dry-run`, and asserts the local file and state file remain absent while stdout contains `would pull <file>`.
2. `hook_dry_run_does_not_pull_or_save_state` writes a project-local `.ferry.toml`, pipes a hook envelope with an absolute missing file path to `hook --cooldown 0 --dry-run`, and asserts the file/state remain absent while stderr contains `would pull`.
3. `forced_pull_dry_run_previews_overwrite_without_writing` seeds a locally changed file and its known state, runs `pull <file> --force --dry-run`, and asserts local/state bytes are unchanged while stderr contains `would overwrite local with remote (--force): <file>`.

Use `Stdio::piped()` and write the hook JSON to stdin:

```rust
let envelope = serde_json::json!({
    "tool_name": "Read",
    "tool_input": { "file_path": target },
});
```

- [ ] **Step 2: Run both tests and verify RED**

Run:

```bash
cargo test --test dry_run_integration pull_file_dry_run_does_not_create_local_file_or_state -- --ignored --nocapture
cargo test --test dry_run_integration hook_dry_run_does_not_pull_or_save_state -- --ignored --nocapture
cargo test --test dry_run_integration forced_pull_dry_run_previews_overwrite_without_writing -- --ignored --nocapture
```

Expected: FAIL because pull and hook still replace/create local files and persist state.

- [ ] **Step 3: Thread mode through pull helpers**

Change these signatures:

```rust
pub fn run(config_path: &Path, paths: &[String], force: bool, mode: ExecutionMode) -> Result<()>
pub fn pull_one(config_path: &Path, rel: &str, force: bool, mode: ExecutionMode) -> Result<bool>
pub fn download_one(/* existing args */, mode: ExecutionMode) -> Result<()>
```

`download_one` returns without calling `write_local_atomic` or updating state in dry-run mode. `pull::run` and `pull_one` skip `state.save` unless `mode.should_apply()`. A `true` result from `pull_one` now means “a pull was required,” whether applied or previewed; update its rustdoc.

Use `would pull <path>` in dry-run output and preserve the existing forced overwrite/conflict behavior. Pass `ExecutionMode::Apply` temporarily from `sync`, `init`, and permanently from `ferry-lsp`.

- [ ] **Step 4: Pass mode through hook and main**

Change `hook::run(cooldown_secs, mode)` and pass it from `main`. Suppress `names::migrate_legacy` inside hook during dry run. Pass mode to `pull_one` and render:

```rust
Ok(true) if mode.is_dry_run() => eprintln!("ferry hook: would pull {rel}"),
Ok(true) => eprintln!("ferry hook: pulled {rel}"),
```

The hook continues converting operational errors into stderr messages and `Ok(())`.

- [ ] **Step 5: Run focused and compatibility tests**

Run:

```bash
cargo test --test dry_run_integration pull_file_dry_run_does_not_create_local_file_or_state -- --ignored --nocapture
cargo test --test dry_run_integration hook_dry_run_does_not_pull_or_save_state -- --ignored --nocapture
cargo test --test dry_run_integration forced_pull_dry_run_previews_overwrite_without_writing -- --ignored --nocapture
cargo test commands::hook
cargo test --bin ferry-lsp
cargo test --test pull_integration --no-run
```

Expected: PASS.

- [ ] **Step 6: Commit the pull/hook slice**

```bash
git add src/main.rs src/commands/pull.rs src/commands/hook.rs src/commands/sync.rs src/commands/init.rs src/bin/ferry-lsp.rs tests/dry_run_integration.rs
git commit -m "fix: make pull and hook dry-run non-mutating"
```

### Task 3: Make Sync Dry-Run Safe

**Files:**
- Modify: `tests/dry_run_integration.rs`
- Modify: `src/main.rs:80-85`
- Modify: `src/commands/sync.rs:20-170`

- [ ] **Step 1: Write the failing bidirectional sync test**

Add `sync_dry_run_preserves_both_sides_and_state`. Seed one local-only file and one remote-only file, save a byte snapshot of any seeded state, run `sync --dry-run`, and assert:

- the local-only file is still absent remotely;
- the remote-only file is still absent locally;
- state bytes/existence are unchanged;
- stdout contains `would upload <local-file>` and `would download <remote-file>`.

- [ ] **Step 2: Run it and verify RED**

Run:

```bash
cargo test --test dry_run_integration sync_dry_run_preserves_both_sides_and_state -- --ignored --nocapture
```

Expected: FAIL because current sync transfers both files and saves state.

- [ ] **Step 3: Thread execution mode through sync**

Change `sync::run(config_path, force, mode)`, pass mode from main, pass mode to `upload_one` and `download_one`, and condition `state.save` on `mode.should_apply()`.

Preserve normal output exactly; dry run emits:

```rust
println!("would upload {rel}");
println!("would download {rel}");
```

For forced conflicts, print `would overwrite remote with local (--force): <path>` without uploading. Without force, keep exit code 2.

- [ ] **Step 4: Verify sync and commit**

Run:

```bash
cargo test --test dry_run_integration sync_dry_run_preserves_both_sides_and_state -- --ignored --nocapture
cargo test --test sync_integration --no-run
```

Expected: PASS/compile.

```bash
git add src/main.rs src/commands/sync.rs tests/dry_run_integration.rs
git commit -m "fix: make sync dry-run non-mutating"
```

### Task 4: Make File and Recursive Remove Dry-Run Safe

**Files:**
- Modify: `tests/dry_run_integration.rs`
- Modify: `src/main.rs:80-85`
- Modify: `src/commands/rm.rs:15-205`

- [ ] **Step 1: Write failing remove tests**

Add two ignored tests:

- `rm_dry_run_preserves_remote_local_and_state` seeds the same file on both sides plus a state record, runs `rm <file> --dry-run`, and asserts all three remain byte-identical while stdout contains `would delete (remote+local) <file>`.
- `recursive_rm_dry_run_preserves_files_and_directories` seeds a nested local/remote subtree, runs `rm <dir> --recursive --dry-run`, and asserts every file and directory remains while stdout previews file deletion and deepest-first directory removal.

- [ ] **Step 2: Run both and verify RED**

Run each filtered test with `--ignored --nocapture`. Expected: FAIL because current rm deletes both sides and drops state.

- [ ] **Step 3: Add mode to rm and its deletion helpers**

Change:

```rust
pub fn run(
    config_path: &Path,
    paths: &[String],
    recursive: bool,
    mode: ExecutionMode,
) -> Result<()>
```

Pass mode through `remove_file_target`, `remove_recursive`, and `delete_file`. In `delete_file`:

```rust
if mode.is_dry_run() {
    println!("would delete ({}) {rel}", sides_label(on_remote, on_local));
    return Ok(());
}
```

Only remove remote/local directories in apply mode. Dry run prints `would remove dir <path>/` in the same deepest-first order. Only save state in apply mode. Preserve the existing path, missing-target, and `--recursive` validation errors.

- [ ] **Step 4: Verify rm and commit**

Run:

```bash
cargo test --test dry_run_integration rm_dry_run_preserves_remote_local_and_state -- --ignored --nocapture
cargo test --test dry_run_integration recursive_rm_dry_run_preserves_files_and_directories -- --ignored --nocapture
cargo test commands::rm
cargo test --test rm_integration --no-run
```

Expected: PASS.

```bash
git add src/main.rs src/commands/rm.rs tests/dry_run_integration.rs
git commit -m "fix: make rm dry-run non-mutating"
```

### Task 5: Make Init Dry-Run Safe

**Files:**
- Modify: `tests/init_integration.rs`
- Modify: `tests/dry_run_integration.rs`
- Modify: `src/main.rs:75-80`
- Modify: `src/commands/init.rs:25-305`

- [ ] **Step 1: Write the non-Docker failing init test**

Add `init_dry_run_does_not_write_config_or_gitignore` beside the existing no-validate test. Run the same prompt script with `init --no-validate --dry-run` and assert:

- exit success;
- config does not exist;
- `.gitignore` does not exist;
- `.ferry/state.json` does not exist;
- stdout contains `would write` and the config path.

- [ ] **Step 2: Run it and verify RED**

Run:

```bash
cargo test --test init_integration init_dry_run_does_not_write_config_or_gitignore -- --nocapture
```

Expected: FAIL because current init writes config and `.gitignore`.

- [ ] **Step 3: Thread mode through init and validation**

Change `init::run(config_path, no_validate, mode)` and `validate_and_resolve(..., mode)`. Pass mode from main.

For user-selected `p`/`P` resolutions, pass mode to `upload_one`/`download_one` and print `would push`/`would pull` in dry mode. Track whether in-sync entries or a `p`/`P` choice would seed state. Save seeded state only in apply mode; in dry mode, print `would write state <path>` when that tracked result is true.

Wrap config-parent creation, config write, and `update_gitignore` in `mode.should_apply()`. Apply mode keeps the existing final message. Dry mode prints:

```rust
writeln!(
    stdout,
    "\nwould write {} and update .gitignore",
    config_path.display()
)?;
```

Do not print the rendered configuration because it contains the plaintext password.

- [ ] **Step 4: Verify the no-validate test is GREEN**

Run both the new dry test and the existing apply-mode test:

```bash
cargo test --test init_integration init_dry_run_does_not_write_config_or_gitignore -- --nocapture
cargo test --test init_integration init_writes_config_and_updates_gitignore -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Add validating-init dry-run coverage**

In `tests/dry_run_integration.rs`, add `validating_init_dry_run_previews_resolution_without_writes`. Seed a differing local/remote file, pipe `p` at the resolution prompt, and assert:

- remote content remains the old remote version;
- local content remains the local version;
- config, `.gitignore`, and state remain absent;
- stdout contains `would push`, `would write state`, and the final config/`.gitignore` preview.

Run it first to confirm RED if validation writes remain, then run again after any minimal correction to confirm GREEN.

- [ ] **Step 6: Commit init behavior**

```bash
git add src/main.rs src/commands/init.rs tests/init_integration.rs tests/dry_run_integration.rs
git commit -m "fix: make init dry-run non-mutating"
```

### Task 6: Suppress Hidden State and Legacy-Name Writes

**Files:**
- Modify: `tests/cli_test.rs`
- Modify: `tests/dry_run_integration.rs`
- Modify: `src/names.rs:1-35`
- Modify: `src/commands/mod.rs`
- Modify: `src/main.rs:60-80`
- Modify: `src/commands/push.rs`
- Modify: `src/commands/pull.rs`
- Modify: `src/commands/sync.rs`
- Modify: `src/commands/rm.rs`
- Modify: `src/commands/status.rs:1-75`

- [ ] **Step 1: Write the failing legacy migration test**

In `tests/cli_test.rs`, create a temp working directory with valid legacy `.zed-ftp.toml` and `.zed-ftp/state.json` but no current names. Point the config at `127.0.0.1:1`, run `status --dry-run` from that directory without `--config`, and assert:

- the command reaches an auth error rather than “config not found”;
- legacy config/state still exist;
- current config/state do not exist.

Expected current failure: `main` migrates both legacy paths before dispatch.

- [ ] **Step 2: Add current-first read-through helpers**

In `src/names.rs`, add:

```rust
pub fn config_path_for_read(dir: &Path) -> std::path::PathBuf {
    let current = dir.join(CONFIG_FILE);
    if current.exists() {
        current
    } else {
        let legacy = dir.join(LEGACY_CONFIG_FILE);
        if legacy.exists() { legacy } else { current }
    }
}
```

In `src/commands/mod.rs`, add `state_path_for(local_root, mode)`. It returns current `.ferry/state.json` normally. In dry mode only, if the current file is absent and legacy `.zed-ftp/state.json` exists, return the legacy file for reading.

Use that state path in push, pull, `pull_one`, sync, rm, and status. Because every dry-run save is already disabled, the fallback path can never be overwritten.

In `main`, choose `names::config_path_for_read(".")` only for dry-run default config lookup; apply mode keeps migration followed by `.ferry.toml`.

- [ ] **Step 3: Run the legacy test and verify GREEN**

Run:

```bash
cargo test --test cli_test dry_run_does_not_migrate_legacy_names -- --nocapture
```

Expected: PASS.

- [ ] **Step 4: Write the failing status cache test**

Add ignored `status_dry_run_does_not_persist_mdtm_cache` to `tests/dry_run_integration.rs`:

- upload a remote file;
- query its actual mtime and size;
- seed a matching state record with `server_supports_mdtm = None`;
- snapshot `state.json` bytes;
- run `status --dry-run`;
- assert normal status output and byte-identical state.

Expected current failure: status changes the in-memory capability to `Some(true)` and saves it.

- [ ] **Step 5: Make status mode-aware and verify GREEN**

Change `status::run(config_path, mode)`, pass mode from main, use `state_path_for`, and save only when `mode.should_apply()`.

Run:

```bash
cargo test --test dry_run_integration status_dry_run_does_not_persist_mdtm_cache -- --ignored --nocapture
cargo test --test status_integration --no-run
cargo test --test cli_test
```

Expected: PASS.

- [ ] **Step 6: Commit hidden-write protections**

```bash
git add src/names.rs src/commands/mod.rs src/main.rs src/commands/push.rs src/commands/pull.rs src/commands/sync.rs src/commands/rm.rs src/commands/status.rs tests/cli_test.rs tests/dry_run_integration.rs
git commit -m "fix: suppress dry-run state and migration writes"
```

### Task 7: Document and Verify the Complete Contract

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add user-facing dry-run documentation**

Near Quick Start or Troubleshooting, document:

```markdown
## Dry runs

Add `--dry-run` anywhere in a Ferry command to inspect the same local and
remote state without changing either side or Ferry's config/state files:

```sh
ferry push src/example.c --dry-run
ferry sync --dry-run
ferry rm old/file.c --dry-run
```

Previewed actions are printed as `would push`, `would pull`, `would upload`,
`would download`, or `would delete`. Validation and conflicts still apply, so
a dry run can fail with the same authentication, path, or conflict exit code
as the real command.
```

- [ ] **Step 2: Run formatting and static checks**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Expected: exit 0 with no warnings. If formatting fails, run `cargo fmt --all`, inspect the mechanical diff, and rerun the checks.

- [ ] **Step 3: Run the full non-Docker suite**

Run:

```bash
cargo test
```

Expected: all non-ignored tests PASS.

- [ ] **Step 4: Run all new real-FTP dry-run tests serially**

Run:

```bash
cargo test --test dry_run_integration -- --ignored --nocapture --test-threads=1
```

Expected: all dry-run integration tests PASS. Confirm each assertion covers remote, local, and state non-mutation as applicable.

The older per-command Docker fixtures have a pre-existing startup/passive-port defect and are not part of this change. Compile them with `cargo test --tests --no-run`; do not claim their ignored runtime suite passes unless those fixtures are repaired separately.

- [ ] **Step 5: Inspect the final diff for accidental scope**

Run:

```bash
git status --short
git diff --stat HEAD~6
git diff HEAD~6 -- src tests README.md
```

Expected: only execution-mode plumbing, guarded mutation sites, dry-run tests/support, and documentation. No behavior changes when `ExecutionMode::Apply` is selected.

- [ ] **Step 6: Commit documentation and any final test-only adjustments**

```bash
git add README.md tests
git commit -m "docs: document dry-run safety contract"
```

- [ ] **Step 7: Run completion verification after the final commit**

Use `@superpowers:verification-before-completion` and rerun:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --test dry_run_integration -- --ignored --test-threads=1
git status --short
```

Expected: every command exits 0, all tests pass, and the worktree is clean.

