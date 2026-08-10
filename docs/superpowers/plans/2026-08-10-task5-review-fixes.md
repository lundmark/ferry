# Task 5 Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Correct the four Task 5 review findings while keeping directory materialization deferred to Task 6.

**Architecture:** Put cancellation checks at the actual guarded staging boundaries, and make production scoped execution the sole source of planning and validation semantics. Production-path tests use a complete in-memory remote implementing the same strict inventory, retrieval, and guarded-write traits as FTP. Type-conflict prefixes are reduced segment-safely and excluded from every descendant planning/validation phase while clean siblings continue.

**Tech Stack:** Rust 2024, anyhow, tempfile, deterministic in-memory FTP trait fake, process CLI fake server.

---

### Task 1: Cancel immediately before guarded staging

**Files:**
- Modify: `src/commands/push.rs`
- Modify: `src/commands/pull.rs`
- Test: `src/commands/push.rs`
- Test: `src/commands/pull.rs`

- [ ] **Step 1: Write upload and download boundary regression tests**

Add sequence gates returning true then false. Each test explicitly consumes the first true result to model `run_scoped_with`'s outer check, then invokes `upload_one_guarded` or `download_one_guarded`; the helper's first boundary check receives false. Assert `CommitDecision::Cancelled`, unchanged destination/state, and no remote upload/temp event or local transfer temp.

- [ ] **Step 2: Run the two focused tests and verify RED**

Run: `cargo test cancels_at_pre_stage_boundary`

Expected: FAIL because each helper stages before consulting `gate.is_current()`.

- [ ] **Step 3: Add the minimal boundary checks**

After payload/source/destination revalidation and immediately before `stage_remote_write_guarded` or `stage_local_write_scoped`, return `Ok(CommitDecision::Cancelled)` when `!gate.is_current()`. Preserve dry-run and the existing claimed closure.

- [ ] **Step 4: Run guarded transfer suites and verify GREEN**

Run: `cargo test commands::push::staging_tests && cargo test commands::pull::staging_tests`

Expected: PASS.

### Task 2: Suppress type-conflict subtrees in production planning

**Files:**
- Modify: `src/commands/sync.rs`
- Test: `src/commands/sync.rs`

- [ ] **Step 1: Add production-path type-conflict tests**

Using `run_scoped_with` and a complete strict remote fake, cover local-directory/remote-file and local-file/remote-directory roots with nonempty descendants plus a clean sibling transfer. Assert one typed parent `SyncIssue::TypeConflict`, committed sibling event/state, deterministic order, and no retrieval/snapshot/stage event below the conflict prefix.

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test commands::sync::tests::structured_type_conflict_subtree`

Expected: FAIL with a descendant probe/snapshot error.

- [ ] **Step 3: Add segment-safe conflict-prefix classification**

Return minimal type-conflict prefixes from inventory-shape classification. Exclude each exact conflict path and every `prefix + "/"` descendant from file planning and directory snapshot capture. Do not suppress near-miss siblings such as `area-old`.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test commands::sync::tests::structured_type_conflict_subtree`

Expected: PASS with clean sibling progress retained.

### Task 3: Replace disconnected planner and cancellation tests

**Files:**
- Modify: `src/commands/sync.rs`
- Test: `src/commands/sync.rs`

- [ ] **Step 1: Build one complete production remote test fixture**

The fake mirrors strict directory listings, file bytes/metadata, stable retrieval, destination snapshots, staged uploads/renames, and an ordered event log. Tests invoke `run_scoped_with`; no test-only business planner is introduced.

- [ ] **Step 2: Port structured semantics to production execution**

Cover all classify mappings, force behavior, state-only absence, type distinction, deterministic event/issue order, cancellation before first boundary, boundary cancellation, between-entry partial commits, and invalidation during real final directory validation on an unchanged scope.

- [ ] **Step 3: Verify each ported test fails for the reviewed gap or passes through production, then remove duplicates**

Delete `HashObservation`, `plan_structured_files`, and tests using dummy `execute_structured_plan` validation closures. Retain only production helpers actually used by `run_scoped_with`.

- [ ] **Step 4: Run all sync tests**

Run: `cargo test commands::sync`

Expected: PASS.

### Task 4: Exercise actual bare CLI dispatch

**Files:**
- Modify: `tests/cli_test.rs`

- [ ] **Step 1: Replace the help-only assertion with a process regression**

Run actual `ferry sync --config <fake>` against a deterministic fake FTP tree containing one nested remote-only file. Assert legacy dispatch materializes its missing parent directory, downloads the file, and saves its state record. Keep the two invalid grammar assertions.

- [ ] **Step 2: Verify the regression catches dispatch drift, then restore and run GREEN**

After writing the test, temporarily route bare sync through structured root dispatch, run `cargo test --test cli_test sync_cli_executes_bare_legacy_sync_and_rejects_invalid_scope_combinations`, and observe failure because structured sync does not materialize missing directories before Task 6. Restore production dispatch and rerun the same command.

Expected final result: PASS and fake server observes legacy dispatch.

### Task 5: Audit persistence, validation, and finish

**Files:**
- Modify as required by prior tasks only; do not implement directory creation.

- [ ] **Step 1: Re-audit partial state save and directory snapshots**

Confirm completed guarded commits are the only file-record changes persisted after a later error/cancellation, conflict descendants do not enter directory validation, and unchanged directories still perform real before/after gate checks.

- [ ] **Step 2: Run required verification**

Run:
- `cargo fmt --all -- --check`
- `cargo test commands::sync`
- `cargo test --test cli_test`
- `cargo test --test sync_integration --no-run`
- `cargo test --test dry_run_integration --no-run`
- `rg -n "println!|eprintln!" src/commands/sync/inventory.rs src/commands/sync/scope.rs src/commands/sync/commit.rs` (expected: no matches)
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets`
- `git diff --check` and artifact/status audit

- [ ] **Step 3: Commit the review fixes**

Commit production/tests/plan in one clean follow-up commit after `b371e5b`, then report exact RED/GREEN evidence and SHA.
