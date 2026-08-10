# Task 5 Quality Review Corrections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make strict current remote content authoritative before scoped uploads, sanitize every scoped FTP hashing operation, and persist scoped state only after file-record commits.

**Architecture:** Keep the generic `run_scoped_with` engine and legacy sync behavior. Add one authoritative-upload reclassification boundary inside the engine, one `ScopedFtp<'_>` adapter around the existing connection for the public route, and one commit-only persistence predicate in `run_scoped`.

**Tech Stack:** Rust, `anyhow`, `suppaftp`, existing fake FTP process harness, existing `ProductionRemote` generic seam.

---

### Task 1: Reclassify upload candidates from strict remote content

**Files:**
- Modify: `src/commands/sync.rs`
- Modify: `src/commands/sync/production_tests.rs`

- [ ] **Step 1: Write three failing production-seam regressions**

Add focused tests named:

- `stale_cached_remote_hash_reclassifies_actual_both_changed_as_conflict`
- `stale_cached_remote_hash_force_uses_actual_destination_snapshot`
- `stale_cached_remote_hash_matching_local_becomes_unchanged_without_upload`

Use same-size `known`, `local`, and actual remote bytes with matching cached MDTM/SIZE. Assert the non-force case reports `FileState::BothChanged` with no upload/rename/file-record mutation; the force case emits `ForcedRemoteOverwrite`, commits local bytes, and necessarily passes guarded verification against the actual strict snapshot; the matching-local case emits `Unchanged` and never stages.

- [ ] **Step 2: Run RED**

Run: `cargo test stale_cached_remote_hash -- --nocapture`

Expected: the first case incorrectly uploads, and the matching-local case incorrectly uploads; the force case exposes that classification used the stale cached SHA.

- [ ] **Step 3: Add an authoritative candidate helper**

Before final action matching, capture one strict destination for every preliminary upload candidate. Reclassify remote-file candidates from the strict `RemoteDestinationSnapshot::File { sha256, size, .. }`; require a true local-only candidate to remain `Missing`; reject file disappearance or type change as a safe planning error. Carry the same captured snapshot into `prepare_upload` so force and normal guarded uploads verify what was actually observed.

Use the reclassified state for the final event/issue/action matrix. Do not strict-snapshot read-only preliminary actions.

- [ ] **Step 4: Run GREEN and focused sync suites**

Run:
- `cargo test stale_cached_remote_hash -- --nocapture`
- `cargo test commands::sync -- --nocapture`

Expected: all pass.

- [ ] **Step 5: Commit**

Commit message: `fix: classify scoped uploads from strict remote content`

---

### Task 2: Route scoped hashing through source-dropping FTP operations

**Files:**
- Modify: `src/ftp.rs`
- Modify: `src/commands/sync.rs`
- Modify: `tests/cli_test.rs`

- [ ] **Step 1: Write failing safe-SIZE and public-route regressions**

Add `scoped_size_transport_error` to the existing mapper matrix test; it must omit the attacker marker, injected second line, ESC, and raw newline while escaping the path.

Extend the fake FTP process harness with hostile MDTM, SIZE, and RETR scenarios for a selected remote file. Drive actual `ferry sync hostile.c --config <fake>` for each scenario. MDTM/SIZE may take their existing compatibility fallbacks; RETR must fail. In every case the rendered stdout/stderr must omit the attacker marker, second-line payload, and ESC byte. The RETR error must retain safe scoped operation context.

- [ ] **Step 2: Run RED**

Run:
- `cargo test ftp::tests::scoped_transfer_errors_drop_every_raw_server_reply_and_escape_paths -- --nocapture`
- `cargo test --test cli_test scoped_sync_hostile_hash_replies_are_not_rendered -- --nocapture`

Expected: safe SIZE support is missing at compile/test time; the RETR route exposes the hostile reply through legacy `Ftp` retrieval.

- [ ] **Step 3: Add `Ftp::size_scoped`**

Implement a source-dropping SIZE mapper parallel to scoped MDTM/RETR. It must call `sanitize_for_message(path)` and discard the `suppaftp::FtpError` source.

- [ ] **Step 4: Add `ScopedFtp<'a>` and wire only `run_scoped`**

Implement `Remote`, `StrictRemote`, `RemoteFileRetrieval`, and `RemoteWrite` for a wrapper borrowing `Ftp`. Delegate LIST, MDTM, SIZE, RETR, upload, rename, remove, destination snapshots, and both dormant MKD methods only to strict/scoped source-dropping operations. Leave `RemoteFileRetrieval for Ftp`, `remote_hash::compute`, and `run_legacy` unchanged.

- [ ] **Step 5: Run GREEN and FTP/CLI suites**

Run:
- `cargo test ftp::tests -- --nocapture`
- `cargo test --test cli_test scoped_sync_hostile_hash_replies_are_not_rendered -- --nocapture`
- `cargo test --test cli_test`

Expected: all pass; no attacker payload is rendered.

- [ ] **Step 6: Commit**

Commit message: `fix: sanitize scoped ftp hash operations`

---

### Task 3: Persist scoped state only after file-record commits

**Files:**
- Modify: `src/commands/sync.rs`
- Modify: `tests/cli_test.rs`

- [ ] **Step 1: Write failing zero-commit process regressions**

Use the real public `run_scoped`/process route to assert:

- an empty root no-op creates no `.ferry` directory;
- a type-only conflict creates no state artifact;
- cancellation before the first claim creates no state artifact;
- cancellation after final directory validation with zero commits creates no state artifact.

Re-export `CommitGate`, `CommitDecision`, and `UnconditionalCommitGate` from `commands::sync` only if required to exercise the existing public `run_scoped` signature consistently from integration tests.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test cli_test scoped_sync_ -- --nocapture`

Expected: zero-commit `Ok` outcomes currently create `.ferry/state.json`.

- [ ] **Step 3: Add partial-progress regressions**

With two selected remote files, add scripted gates proving:

- the first committed download is saved when cancellation occurs before the second action;
- the first committed download is saved when the second gate claim returns an injected error.

Retain the existing clean-sibling/type-conflict disk-state regression. Assert the uncommitted second file is absent both locally and from state.

- [ ] **Step 4: Change the persistence predicate**

Set `should_save` solely from `state.files != initial_files`; keep the existing `mode.should_apply()` guard and the existing combined execution/save error handling. Capability-only probing may mutate memory but must not create or rewrite state.

- [ ] **Step 5: Run GREEN and guarded suites**

Run:
- `cargo test --test cli_test scoped_sync_ -- --nocapture`
- `cargo test commands::pull::staging_tests -- --nocapture`
- `cargo test commands::push::staging_tests -- --nocapture`

Expected: all pass.

- [ ] **Step 6: Commit**

Commit message: `fix: save scoped state only after commits`

---

### Task 4: Audit, verify, and re-review

**Files:**
- Modify only files required by Tasks 1–3 and this plan.

- [ ] **Step 1: Run focused and compile-only verification**

Run:
- `cargo test commands::sync`
- `cargo test --test cli_test`
- `cargo test ftp::tests`
- `cargo test commands::pull::staging_tests`
- `cargo test commands::push::staging_tests`
- `cargo test --test sync_integration --no-run`
- `cargo test --test dry_run_integration --no-run`

- [ ] **Step 2: Run safety audits**

Run:
- `rg -n "println!|eprintln!" src/commands/sync/inventory.rs src/commands/sync/scope.rs src/commands/sync/commit.rs` (expect no matches)
- `rg --files -g '*.orig' -g '*.rej' -g '*~' .` (expect no artifacts)
- `git diff --check`
- confirm production `sync.rs` still has no directory-creation calls.

- [ ] **Step 3: Run full verification**

Run:
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets`
- `cargo test --doc`

- [ ] **Step 4: Request independent code review**

Review the complete corrective range against `docs/superpowers/specs/2026-08-10-task5-quality-review-corrections-design.md`. Fix every Critical/Important finding and re-run affected verification.

- [ ] **Step 5: Finalize**

Confirm the worktree contains only intentional commits, report RED/GREEN evidence, commit SHAs/files, verification counts, and that Task 6 remains untouched.
