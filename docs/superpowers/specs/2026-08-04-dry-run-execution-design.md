# Dry-Run Execution Design

## Problem

`ferry` exposes a global `--dry-run` flag, but `main` parses the flag and never
uses it. As a result, commands run exactly as they do without the flag. The
reported case, `ferry push <file> --dry-run`, uploads the file and persists its
new sync state.

The flag has been disconnected since the original CLI skeleton. The recent
single-file path-resolution change did not introduce the bug.

## Contract

`--dry-run` is a global no-write contract. A dry-run command may read local
files, connect to the configured server, list and download remote files for
classification, and report errors. It must not:

- create, replace, rename, or delete local project files or directories;
- upload, rename, create, or delete remote files or directories;
- create or update the ferry config, `.gitignore`, or sync state;
- rename legacy `.zed-ftp` config or state into the current `.ferry` names.

Commands still perform normal validation and classification so the result is a
faithful preview. Conflicts keep their normal behavior: without `--force`, a
dry run reports the conflict and exits with code 2; with `--force`, it reports
the overwrite it would perform and exits successfully.

## Execution Model

Add a small `ExecutionMode` enum at the command-module boundary:

```rust
pub enum ExecutionMode {
    Apply,
    DryRun,
}
```

`main` converts `Cli::dry_run` into this mode once and passes it to commands
that can write. An enum is preferred to a positional boolean because command
signatures and call sites state their intent. Shared mutation helpers receive
the mode too, so callers such as `push`, `sync`, `init`, `hook`, and the LSP do
not each reinvent the safety check. Non-CLI callers, including `ferry-lsp`,
explicitly use `ExecutionMode::Apply` and retain their current behavior.

The command continues to build the same target set, hashes, and file-state
classification in either mode. At the mutation boundary:

- `Apply` performs the current operation and updates in-memory state.
- `DryRun` skips the operation and leaves in-memory state unchanged.

Every command-level state save is also conditional on `Apply`, including
metadata-only changes such as the cached `server_supports_mdtm` decision.

## Command Behavior

| Command | Dry-run behavior |
|---|---|
| `push` | Classify normally; print `would push <path>` or a forced-overwrite preview; do not upload, create remote parents, or save state. |
| `pull` | Classify normally; print `would pull <path>` or a forced-overwrite preview; do not create or replace local files or save state. |
| `sync` | Classify normally; print `would upload <path>` or `would download <path>`; do not modify either side or save state. |
| `rm` | Resolve files and recursive subtrees normally; print the files and directories it would remove; do not remove either side or state records. |
| `init` | Prompt and optionally validate normally. Report config, state, `.gitignore`, push, and pull actions without performing them. |
| `hook` | Honor parsing and cooldown checks, classify the referenced file, and report `would pull` when appropriate; do not migrate names, write the file, or save state. It still always exits successfully. |
| `status` | Produce the normal status report but do not persist the MDTM capability cache. |
| `ls` | Already read-only; behavior is unchanged. |
| `cc` / `check` | The remote service performs a pure compile check; behavior is unchanged. |

Normal-mode wording and behavior remain unchanged. Dry-run output uses
future-tense wording (`would push`, `would pull`, `would delete`, and so on) so
scripts and users cannot mistake a preview for a completed action.

## Legacy Config and State

Normal execution retains the automatic legacy-name migration. Dry-run mode
must not rename those files. When the default current config or state path is
absent, dry run reads the corresponding legacy path in place. Current names
continue to take precedence when both exist.

This fallback is read-only. A later normal command can still perform the
existing migration.

## Error Handling

Dry run preserves normal errors that can be discovered without applying the
mutation: invalid paths, missing files, authentication failures, tree-walk
errors, and conflicts. It does not claim an action succeeded; it only states
that the action would be attempted. Apply-only failures, such as a server
rejecting an upload or the local filesystem rejecting a rename, cannot be
predicted and are not synthesized.

For recursive removal, dry run reports directory-removal attempts after the
files beneath them. It does not warn that a removal failed because it never
attempts the removal.

## Testing

Implementation follows test-first development:

1. Add the reported regression case for `push <file> --dry-run`. Verify the
   remote bytes and serialized state are unchanged and the output says
   `would push`.
2. Add equivalent non-mutation coverage for pull, sync, and rm, checking local
   files, remote files, and state as applicable.
3. Cover `init --dry-run --no-validate` without FTP: no config or `.gitignore`
   is created and the output previews the write.
4. Cover hidden writes: dry-run status does not persist state-cache changes,
   and default-path dry runs do not migrate legacy config or state.
5. Cover hook dry-run behavior and ensure the LSP and normal CLI paths still
   select `Apply`.
6. Run focused tests after each red/green cycle, then the full non-Docker test
   suite. Run Docker-gated FTP cases when the environment supports the
   repository's test container fixture; otherwise retain them as explicit
   integration coverage and report the existing environment limitation.

## Out of Scope

- Refactoring every command into a separate pure planning engine and executor.
- Changing `--force`, conflict classification, ignore matching, or path scope.
- Giving `--verbose` behavior; it is a separate currently-unused global flag.
- Predicting failures that only an attempted write can reveal.
