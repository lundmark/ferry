# Guarded Transfer Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:test-driven-development for each behavior change and
> superpowers:verification-before-completion before reporting success.

**Goal:** Give each guarded transfer an independently owned, revalidated temp
and prevent scoped FTP errors from exposing raw server replies.

**Architecture:** A shared exact temp-name module generates 128-bit sibling
names and recognizes only the reserved protocol grammar. Scoped inventory
uses the recognizer. Pull captures local identity and content; push captures a
strict remote snapshot. Each is revalidated in its final commit claim, and
cleanup is conditional on retained ownership. Production `Ftp` implements the
existing scoped traits through fresh sanitized error adapters while legacy
public methods remain unchanged.

---

### Task 1: Specify exact scoped inventory reservation

1. Add failing local and remote tests for exact stale-temp exclusion and
   near-miss inclusion.
2. Implement the shared exact recognizer and use it only in scoped inventory.
3. Run focused inventory tests.

### Task 2: Specify unique local temp ownership

1. Add failing tests for unique sibling names, claim-time mutation/replacement,
   interleaved writers, and ownership-safe cleanup.
2. Generate candidates with `getrandom` 0.4 and create local temps exclusively.
   Retry occupied candidates through a test-injectable candidate source.
3. Capture and revalidate identity/type/size/mtime/hash inside the claim.
4. Run focused pull and commit tests.

### Task 3: Specify unique remote temp ownership

1. Add failing tests for unique sibling names, strict temp mutation/replacement,
   interleaved writers, cleanup ownership, and rename/state ordering.
2. Require Missing, upload, then capture the strict temp snapshot.
   Retry occupied candidates through the same deterministic test seam.
3. Re-snapshot inside the claim immediately before rename; clean only an exact
   owned snapshot.
   Before ownership is established, clean upload/capture failures only after a
   fresh strict snapshot proves the intended regular-file size and hash.
4. Compute upload `last_synced` after successful rename immediately before
   state insertion.
5. Run focused push and file-transfer tests.

### Task 4: Sanitize scoped FTP operation failures

1. Add failing attacker-reply regressions for scoped download, upload, mtime,
   rename, and remove.
2. Add scoped-only adapters that discard every raw `suppaftp` error, including
   server/protocol replies. Route `StrictDestinationRead::download_destination`
   and the scoped download/upload/mtime/rename/remove production trait methods
   through them.
3. Leave all legacy public methods untouched and run focused FTP tests.

### Task 5: Verify and commit

1. Update `CommitGate` semantic documentation.
2. Run `cargo fmt --all -- --check`, focused suites, strict all-target/all-feature
   Clippy, and full `cargo test`.
3. Audit `git diff --check`, changed files, lockfile dependency scope, and
   generated artifacts.
4. Create logical commits and confirm the worktree is clean.
