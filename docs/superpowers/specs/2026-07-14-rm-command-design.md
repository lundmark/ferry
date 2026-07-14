# `ferry rm` — delete files on the server (and locally)

Date: 2026-07-14

## Problem

`ferry` can `push`, `pull`, `sync`, and `status`, but it has no way to
delete a file on the remote server. Deletion was deliberately kept out of
`push` — `push.rs` already documents that a locally-missing file is "not
yours to delete" and that `rm` is "its own deliberate command." This spec
fills that gap.

## Goals

- Delete a file on the FTP server, its local mirror copy, and its `state.json`
  entry in one deliberate command.
- Support recursive deletion of a directory subtree behind an explicit flag.
- Be safe by construction: no accidental mass deletion.

## Non-goals

- No propagation of deletions through `push`/`pull`/`sync` (those remain
  non-destructive-by-omission).
- No interactive confirmation prompt — the command deletes immediately when
  invoked (matching the fire-and-do-it style of `push`/`pull`), relying on
  explicit paths and the `--recursive` gate for safety.
- No "delete everything" mode.

## Command surface

```
ferry rm <paths...> [--recursive]
```

- New `Rm { paths: Vec<String>, recursive: bool }` subcommand in `main.rs`,
  dispatched to `commands::rm::run(&cfg, &paths, recursive)`.
- **Bare `rm` (no paths) is an error**: `rm requires at least one path`
  (exit 1). This is the primary guard against accidental mass deletion.
- Paths are relative to the roots, exactly as in `push`/`pull`: normalized to
  forward slashes, and **rejected if absolute or containing a `..` segment**.
- Each target is deleted on **remote + local + state**.

## Behavior

### Non-recursive (single file)

- Presence is determined per side without relying on ambiguous FTP 550
  replies: `ftp.size(remote_path).is_ok()` → remote present;
  `local_full.is_file()` → local present.
- If the path is a directory (locally `is_dir`, or remote-only where `size`
  fails but a listing shows a directory) → error:
  `refusing directory "<p>": pass --recursive`.
- If present on **neither** side → error `no such file on remote or local:
  <p>` (exit 1).
- Otherwise delete whichever sides have it, remove the state entry, and print
  `deleted (remote+local) <rel>` (or `(remote)` / `(local)` when only one
  side had the file).

### Recursive (`--recursive`, directory)

- Enumerate the subtree on both sides: `walk_remote` scoped to the arg
  (already tolerant of broken remote directories) ∪ `walk_local` scoped to the
  arg (honors the ignore matcher).
- Delete every file in the union — remote `rm` for files the remote walk
  found, local `remove_file` for files present locally, and remove the state
  entry — printing one line per file.
- Then remove now-empty directories **bottom-up** (deepest first): remote
  `rmdir`, and local `remove_dir` (only when empty). Failures here are
  **warnings, not fatal** — e.g. a directory still holding ignored local files
  is left in place, which is the safe outcome. Print `removed dir <rel>/` on
  success.

## Implementation

- **`src/ftp.rs`**: add `rmdir(&mut self, path: &str)` wrapping suppaftp's
  `rmdir` (present in the crate). `rm` already exists.
- **`src/commands/walk.rs`**: promote `push`'s private `normalize_rel` plus the
  `..`/absolute validation into a shared `pub fn` here; both `push` and `rm`
  use it (removes duplication rather than adding a second copy). Both callers
  are test-covered, so the refactor is low-risk.
- **`src/commands/mod.rs`** + **`src/main.rs`**: register the command.

## Error / exit codes

- Config/auth failures already map to exit 3 via `Config::load` /
  `Ftp::connect`.
- Everything else (usage error, delete failure) → generic exit 1.
- No conflict (exit 2) semantics — `rm` never refuses on divergence; it is a
  deliberate destructive command.

## Zed + docs

- Add `FTP: delete current file` to `examples/tasks.json`
  (`args: ["rm", "$ZED_RELATIVE_FILE"]`, `reveal: on_error`).
- Update `README.md` command list and `NEWS`.

## Testing

- **`tests/cli_test.rs`** (no Docker): `rm` appears in `--help`; bare `rm`
  exits non-zero; `rm ../escape` and absolute paths are rejected.
- **`tests/rm_integration.rs`** (Docker-gated, `#[ignore]`, mirroring
  `push_integration.rs`):
  - single-file delete removes remote + local + state;
  - recursive delete clears a seeded subtree and `rmdir`s the emptied
    directories;
  - deleting a remote-only file (no local copy) still succeeds;
  - non-recursive on a directory errors.

## Judgment calls

- **Ignored local files block their parent directory's removal** in recursive
  mode (warn, leave the directory) — safety over completeness.
- **The `normalize_rel` refactor touches `push.rs`** — accepted to keep a
  single source of truth for path validation.
