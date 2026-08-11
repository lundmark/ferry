# Final Transfer Ownership Checks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate unproven local cleanup and make guarded upload state and claim validation use exact MDTM without weakening strict-snapshot ownership.

**Architecture:** Local staging gains a test-injectable identity-capture seam; capture failure returns without unlinking the unproven reserved path, while normal captured-identity cleanup remains unchanged. Guarded remote staging keeps the full strict LIST/size/hash snapshot as its ownership record, separately captures exact MDTM through `RemoteWrite::mtime`, revalidates both inside the claim, and records the exact MDTM without any post-rename probes.

**Tech Stack:** Rust 2024, anyhow, chrono, same-file, Ferry's `RemoteWrite` and `CommitGate` seams.

---

### Task 1: Local identity-capture failure is non-destructive

**Files:**
- Modify: `src/commands/pull.rs:380-485`
- Test: `src/commands/pull.rs` `staging_tests`

- [ ] **Step 1: Add a deterministic failing test**

Add an identity-capture seam used only by the inner staging helper. In the test closure, unlink the just-created temp, write foreign replacement bytes at the same reserved path, then return an injected `std::io::Error`. Assert staging returns the identity-capture error and the foreign replacement remains.

- [ ] **Step 2: Run the test and verify RED**

Run: `cargo test identity_capture_failure_preserves_unproven_replacement`

Expected: FAIL because the current error branch unconditionally removes the replacement pathname.

- [ ] **Step 3: Implement the minimal cleanup correction**

On identity-capture error, close the still-open file and return contextual error without calling `remove_file`. Keep production capture as `file.try_clone().and_then(Handle::from_file)`; retain the existing identity-checked `Drop` behavior after capture succeeds.

- [ ] **Step 4: Verify local GREEN**

Run:
- `cargo test identity_capture_failure_preserves_unproven_replacement`
- `cargo test commands::pull::staging_tests`

Expected: injected regression and all local ownership tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/commands/pull.rs
git commit -m "fix: preserve temps without local identity proof"
```

### Task 2: Capture exact MDTM after strict remote ownership

**Files:**
- Modify: `src/commands/push.rs:399-640`
- Test: `src/commands/push.rs` `staging_tests`

- [ ] **Step 1: Add failing LIST-versus-MDTM and staging-failure tests**

Extend `FakeRemote` so strict snapshots read a coarse LIST timestamp while `mtime` returns independently scripted exact MDTM values. Add tests proving:
- state records exact MDTM when LIST and MDTM differ;
- an MDTM failure after strict ownership removes an unchanged owned temp through exact snapshot equality;
- the same failure preserves a replacement observed by cleanup.

- [ ] **Step 2: Run and verify RED**

Run: `cargo test guarded_remote_.*mtime` using individual exact test filters if Cargo filtering requires them.

Expected: state currently records LIST time, and guarded staging currently performs no MDTM probe/failure cleanup.

- [ ] **Step 3: Implement exact MDTM staging**

Keep `temp_snapshot` unchanged as the full strict ownership record. After it matches intended size/hash, call `remote.mtime(&temp_path)`. Store the returned exact value in `StagedRemoteWrite.modified`. If MDTM fails, call `cleanup_remote_snapshot` with the already captured strict snapshot and return contextual error.

- [ ] **Step 4: Verify staging GREEN**

Run the three exact test filters and `cargo test commands::push::staging_tests`.

Expected: exact state time and ownership-safe MDTM-failure cleanup pass.

### Task 3: Revalidate exact MDTM inside the claim

**Files:**
- Modify: `src/commands/push.rs:590-640`
- Test: `src/commands/push.rs` `staging_tests`

- [ ] **Step 1: Add failing claim-time tests**

Script stage-time MDTM followed by either a changed MDTM or an MDTM error at claim time. Assert no rename, destination/state preservation, and exact-snapshot-safe temp cleanup. Extend the success ordering test to assert one pre-rename staging MDTM probe, one pre-rename claim MDTM probe, state uses exact MDTM, and no snapshot/MDTM event follows rename.

- [ ] **Step 2: Run and verify RED**

Run the exact claim-time change, error, and success-order tests.

Expected: current claim checks only the strict snapshot and therefore commits through exact-MDTM changes/errors.

- [ ] **Step 3: Implement claim revalidation**

After exact strict-snapshot equality and before rename, call `remote.mtime(&staged.temp_path)`; require equality with staged exact MDTM. Propagate sanitized errors. Leave existing outer cleanup unchanged so it removes only when the strict owned snapshot still matches.

- [ ] **Step 4: Verify remote GREEN and commit**

Run: `cargo test commands::push::staging_tests`

Then:
```bash
git add src/commands/push.rs
git commit -m "fix: track exact mtime for guarded uploads"
```

### Task 4: Documentation and full verification

**Files:**
- Modify: `docs/superpowers/plans/2026-08-10-guarded-transfer-ownership.md`
- Verify: all files changed since `8fdf8e3`

- [ ] **Step 1: Update the implementation record**

Record local no-unlink-before-identity proof, strict snapshot plus exact MDTM separation, claim-time dual validation, no post-rename probes, and the manual-only historical `TARGET.tmp.zedftp` cleanup policy. Do not change scoped inventory matching.

- [ ] **Step 2: Run all verification**

Run:
- `cargo fmt --all -- --check`
- focused pull, push, FTP, temp, inventory, file-transfer, and gate suites;
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-targets`
- `git diff --check`
- `git status --short --untracked-files=all`

Expected: all enabled tests pass, configured ignored tests remain ignored, no warnings, no generated artifacts, and only intended files differ.

- [ ] **Step 3: Commit documentation and confirm clean worktree**

```bash
git add docs/superpowers/plans/2026-08-10-guarded-transfer-ownership.md
git commit -m "docs: record final transfer ownership checks"
git status --short
```
