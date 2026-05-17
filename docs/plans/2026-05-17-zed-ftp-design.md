# zed-ftp — Design

**Date:** 2026-05-17
**Status:** Approved, ready for implementation planning

## Summary

`zed-ftp` is a standalone Rust CLI that gives Zed editor users an FTP sync workflow built around local-mirror-with-manual-trigger. Files are edited locally; explicit `push`/`pull`/`sync` commands move them over plain FTP. Triggered from Zed via `.zed/tasks.json` entries — no Zed extension manifest is required, because the chosen trigger model (the task runner) already invokes external commands directly.

Per-file SHA-256 checksums in a local state file detect drift and refuse to silently overwrite a remote that has changed since the last known sync.

## Why a CLI and not a Zed extension package

Zed's extension API (slash commands, language servers, themes, context servers, snippets, indexed docs) does not expose a generic filesystem-provider or save-hook API. The realistic ways to wire sync actions into Zed are either slash commands inside the assistant panel (awkward for sync) or shell-out via `.zed/tasks.json`. The task-runner route is cleaner, works today, and doesn't require any extension packaging. We ship a CLI plus a documented `tasks.json` template.

## Scope decisions

| Decision | Choice | Rationale |
|---|---|---|
| Workflow | Local mirror + manual sync | Fits Zed's API constraints; matches user mental model |
| Trigger | External CLI invoked via `.zed/tasks.json` | Idiomatic for Zed, no extension packaging needed |
| Protocol | FTP plain only | Out of scope: FTPS, SFTP |
| Conflicts | Per-file SHA-256 tracking | Robust against mtime lies and concurrent edits |
| Config | Project-local `.zed-ftp.toml` (password included, gitignored) | Simplest first version; keychain integration left for later |

## Architecture

Single Rust binary crate `zed-ftp`. Layout:

```
zed_ftp/
├── Cargo.toml
├── src/
│   ├── main.rs           # clap arg parsing, command dispatch
│   ├── config.rs         # load/validate .zed-ftp.toml
│   ├── state.rs          # read/write .zed-ftp/state.json; classification logic
│   ├── ftp.rs            # thin wrapper over suppaftp
│   ├── ignore.rs         # gitignore-style pattern matching
│   └── commands/
│       ├── init.rs
│       ├── pull.rs
│       ├── push.rs
│       ├── sync.rs
│       └── status.rs
├── examples/
│   └── tasks.json
└── README.md
```

Key dependencies: `suppaftp` (sync FTP client), `clap` (args), `serde` + `toml` (config), `serde_json` (state), `sha2` (hashes), `ignore` or `globset` (ignore patterns), `anyhow` (errors). Synchronous I/O — FTP runs on a single control connection, async adds complexity for no benefit.

## CLI surface

```
zed-ftp init                 # interactive setup; validates existing local files vs remote
zed-ftp status               # categorize every file: in-sync, local-changed, remote-changed, conflict, ...
zed-ftp pull [PATH]...       # download remote → local
zed-ftp push [PATH]...       # upload local → remote
zed-ftp sync                 # pull then push; refuses on conflicts unless --force
```

Global flags: `--config <path>` (default `./.zed-ftp.toml`), `--verbose`, `--dry-run`.

Exit codes:
- `0` success
- `1` generic error
- `2` conflict detected
- `3` config/auth problem

`push <PATH>` and `pull <PATH>` accept a file or directory subtree — the tasks.json template uses `$ZED_RELATIVE_FILE` to operate on the currently open file.

### `init` flow

```
1. Prompt for host / user / password / remote-root
2. Connect and verify auth
3. Walk local dir + walk remote tree
4. Classify every path:
     - in-sync       (exists both, sha256 matches)
     - local-only    (exists local, missing remote)
     - remote-only   (exists remote, missing local)
     - differs       (exists both, hashes mismatch)
5. Print summary table; for `differs` offer:
     [k]eep both unsynced  [p]ush local  [P]ull remote  [s]kip per-file decision
6. Write .zed-ftp.toml and seed state.json with hashes for in-sync entries only
     (differing/local-only/remote-only stay outside trusted state until resolved)
```

Hashing remote files means downloading them once during init (FTP has no server-side hash). Progress is printed; `--no-validate` skips the deep comparison if the user just wants the config written.

## Config format

`.zed-ftp.toml` at project root:

```toml
[connection]
host = "ftp.example.com"
port = 21                    # optional, default 21
user = "deploy"
password = "..."             # plaintext; init adds file to .gitignore
passive = true               # default true

[paths]
local_root  = "."            # relative to config file
remote_root = "/var/www/site"

[sync]
ignore = [                   # gitignore syntax via the `ignore` crate
  "node_modules/",
  "target/",
  ".git/",
  ".zed-ftp/",
  "*.log",
]

# Optional allowlist mode:
# include = ["src/", "public/"]
```

State sits beside it in `.zed-ftp/state.json` (also gitignored):

```json
{
  "version": 1,
  "files": {
    "src/index.html": {
      "sha256": "abc123...",
      "size": 4821,
      "remote_mtime": "2026-05-17T08:12:33Z",
      "last_synced": "2026-05-17T08:15:01Z"
    }
  }
}
```

`init` auto-appends `.zed-ftp.toml` and `.zed-ftp/` to `.gitignore`. Plaintext password is a known footgun — flagged at init prompt and in the README.

## Conflict semantics

The state file is the source of truth for "what we last knew about each file." On every operation:

```
hash_local  = sha256(local file)  or  None
hash_remote = sha256(download F)  or  None
hash_known  = state[F].sha256     or  None
```

Classification:

| State | Meaning |
|---|---|
| in-sync | `hash_local == hash_remote == hash_known` |
| local-changed | `hash_local != hash_known`, `hash_remote == hash_known` |
| remote-changed | `hash_local == hash_known`, `hash_remote != hash_known` |
| both-changed | both differ from `hash_known` → conflict |
| local-only | `hash_remote is None` |
| remote-only | `hash_local is None` |
| untracked | `hash_known is None`, both exist |

Action matrix:

| State | `pull` | `push` | `sync` |
|---|---|---|---|
| in-sync | noop | noop | noop |
| local-changed | warn, overwrite local with `--force` | upload | upload |
| remote-changed | download | warn, requires `--force` | download |
| both-changed | `--force` overwrites local | `--force` overwrites remote | refuse; exit 2 |
| local-only | noop | upload (new) | upload |
| remote-only | download (new) | noop | download |
| untracked | as if both-changed | as if both-changed | refuse; exit 2 |

**Optimization:** before hashing remote, compare `MDTM` + `SIZE` to `state[F].remote_mtime`/`size`. If both match, trust `hash_known` as `hash_remote` and skip the download. Steady-state syncs only hash files whose metadata moved.

State updates after each successful transfer: new sha256, fresh mtime, fresh `last_synced`.

## Zed tasks.json template

Shipped as `examples/tasks.json`; users copy into `.zed/tasks.json`:

```json
[
  {
    "label": "FTP: push current file",
    "command": "zed-ftp",
    "args": ["push", "$ZED_RELATIVE_FILE"],
    "use_new_terminal": false,
    "reveal": "on_error"
  },
  {
    "label": "FTP: pull current file",
    "command": "zed-ftp",
    "args": ["pull", "$ZED_RELATIVE_FILE"],
    "reveal": "on_error"
  },
  {
    "label": "FTP: status",
    "command": "zed-ftp",
    "args": ["status"],
    "reveal": "always"
  },
  {
    "label": "FTP: sync all",
    "command": "zed-ftp",
    "args": ["sync"],
    "reveal": "always"
  }
]
```

Invoked via command palette → `task: spawn` → pick label. Exit code 2 surfaces in the terminal panel via `reveal: on_error`.

## Testing strategy

- **Unit tests:** config parsing, ignore matching, state file round-tripping, the pure classification function `(hash_local, hash_remote, hash_known) → State`.
- **Integration tests** against a real FTP server in Docker (`fauria/vsftpd` or `delfer/alpine-ftp-server`) via the `testcontainers` crate. Cover init on empty + populated directories (all four init categories), push/pull/sync across every classification, conflict refusal, `--force` overrides, ignore patterns.
- **No mocking** the FTP layer — protocol-level edge cases (passive mode, listing format variation, MDTM support) are exactly where mocked tests would lie.

## Error handling

`anyhow::Result` with context throughout; error categories map to exit codes (1/2/3) so Zed tasks reveal failures correctly.

- **Partial transfers:** upload to `path.tmp.zedftp`, rename atomically; on failure, leave original intact and clean up the temp file.
- **Server lacks MDTM/SIZE:** fall back to always hashing (slow path); log a one-time warning into `.zed-ftp/state.json` metadata so we don't re-detect every run.
- **Network drop mid-walk:** every command is restartable. State updates only on successful transfers; re-running picks up where it left off.
- **Auth failure:** exit 3, point user at config.

## Out of scope (future)

- FTPS / SFTP
- OS keychain integration for credentials
- Save-on-edit watcher
- Bidirectional auto-merge of conflicting files
- Zed slash-command extension wrapping the CLI (could be added as v2)
