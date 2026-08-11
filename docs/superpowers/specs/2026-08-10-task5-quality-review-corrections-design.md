# Task 5 Quality Review Corrections Design

## Goal

Close three scoped-sync safety gaps without changing legacy sync semantics or implementing Task 6 directory creation: strict remote content must govern every upload decision, scoped FTP metadata/content failures must not expose raw server replies, and state must be persisted only after a file-record commit.

## Non-goals

- Do not create local or remote directories in structured scoped sync.
- Do not change legacy `ferry sync` hashing or error-detail behavior.
- Do not change directory identity/root hardening or historical temp-file policy.
- Keep `run_scoped` public; re-export internal gate types only if a production-route test genuinely requires it.

## Authoritative upload decisions

`remote_hash::compute_with` remains the initial classifier so scoped sync retains its MDTM/SIZE cache fast path. Before scheduling any preliminary upload candidate (`LocalChanged`, `LocalOnly`, or forced `BothChanged`/`Untracked`), scoped sync captures the strict destination snapshot.

For an inventory entry that was a remote file, the strict snapshot must still be a file. Its SHA-256 and size become authoritative: scoped sync re-runs `classify(local, strict_remote, known)` and uses that result for the final action. A stale cached `LocalChanged` may therefore become `InSync` (no transfer) or `BothChanged` (conflict unless forced). A forced conflict may still overwrite, but the guarded upload carries the exact strict snapshot it observed. Missing or type-changing destinations fail safely instead of being treated as authorization to recreate or replace them.

For a true local-only inventory entry, the strict destination must remain `Missing`; any appearance or type change is a planning conflict. The existing guarded claim-time snapshot verification remains the final mutation boundary.

## Scoped FTP adapter

Add a `ScopedFtp<'a>` adapter used only by `run_scoped`. It implements `Remote`, `StrictRemote`, `RemoteFileRetrieval`, and `RemoteWrite` by delegating exclusively to source-dropping scoped operations.

`Ftp` gains `size_scoped`, matching existing safe MDTM, RETR, upload, rename, remove, strict LIST, and strict MKD methods. Error mappers discard the `suppaftp` source and render paths with `escape_default`, so formatted error chains cannot contain attacker reply text, newlines, or terminal escape bytes. The legacy `RemoteFileRetrieval for Ftp` remains unchanged and `run_legacy` continues using raw legacy methods.

## Commit-only persistence

`run_scoped` snapshots `state.files` before execution. In apply mode it saves only when `state.files` differs afterward, regardless of whether execution returned `Ok` or `Err`. Capability-only changes such as `server_supports_mdtm`, zero-commit cancellation, type-only conflict, stale/no-op outcomes, and dry-run must not create `.ferry` or `state.json`.

If an earlier clean transfer committed a file record before a later conflict, cancellation, or error, the changed `state.files` map is still saved. This preserves completed progress while making persistence an observable consequence of a committed file mutation.

## Testing

- Production `run_scoped_with` regressions script stale cache metadata with same-size changed remote bytes. Without force the actual strict SHA yields `BothChanged`, no staging, and no file-record mutation; with force the upload succeeds only against the actual captured snapshot and emits `ForcedRemoteOverwrite`; when strict remote bytes equal local bytes, the preliminary upload becomes `Unchanged` with no staging.
- Public/process regressions drive real `run_scoped` through a fake FTP server with hostile MDTM, SIZE, and RETR replies. Output/error chains must omit the marker, raw newline, and ESC bytes while retaining sanitized operation context.
- Persistence regressions assert no state directory/file for zero-commit successful or cancelled outcomes and assert saved file records after partial clean progress followed by conflict/cancellation/error.
- Run focused sync, CLI, FTP, and guarded-transfer suites; integration targets with `--no-run`; no-print and artifact audits; formatting; strict Clippy; and all targets.

## Success criteria

Every upload is classified and guarded against strict current remote content, scoped FTP failures are safe to render with `{:#}`, zero-commit scoped runs leave no state artifact, partial commits remain durable, all existing legacy behavior and tests remain intact, and Task 6 remains out of scope.
