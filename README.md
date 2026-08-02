# ferry

A small Rust CLI for keeping a local project tree in sync with an FTP server —
`push`, `pull`, `sync`, `status`, `rm`, and `init` (plus `cc` for remote
compile-checks against a MUD-style `check_compile` service). It's designed to
be driven from an editor (e.g. [Zed](https://zed.dev)'s `.zed/tasks.json`) or
from coding-agent PreToolUse hooks (Claude Code, Codex), so files are pulled
and pushed as you and your agents work.

> **Renamed from `zed-ftp`.** The binary is now `ferry`, the config file is
> `.ferry.toml`, and state lives in `.ferry/`. Existing projects are migrated
> automatically: the first time `ferry` runs in a project that still has
> `.zed-ftp.toml` / `.zed-ftp/`, it renames them in place. Update any hook or
> task wiring that referenced the old `zed-ftp` / `zed-ftp-lsp` binaries.

## Status

Functional and unit-tested. The FTP integration tests are gated behind a live
Docker daemon (they spin up a real `vsftpd` container), and the end-to-end
smoke test of the editor `tasks.json` flow has **not yet been run** in this
environment because Docker was unavailable. Expect to verify the round-trip
against your own server before relying on it.

## Installation

From a checkout of this repo:

```sh
cargo install --path .
```

This drops a `ferry` binary into `~/.cargo/bin`.

## Quick start

In your project root:

```sh
ferry init
```

The wizard prompts for host, username, password, and remote root, then
validates by listing the remote root and walking it against your local tree.
Use `--no-validate` to skip the remote walk if you just want the config file
written:

```sh
ferry init --no-validate
```

This writes a `.ferry.toml` to the project root and appends it to
`.gitignore`.

## Tasks.json integration

Copy [`examples/tasks.json`](examples/tasks.json) into your project's
`.zed/tasks.json` (or merge the entries with your existing tasks). Then in
Zed, open the command palette and run `task: spawn` to pick one of:

- `FTP: push current file`
- `FTP: pull current file`
- `FTP: delete current file`
- `FTP: status`
- `FTP: sync all`

The per-file tasks use Zed's `$ZED_RELATIVE_FILE` variable so they operate on
whichever buffer is active.

`FTP: delete current file` runs `ferry rm`, which removes the file on the
server **and** the local copy (and drops its sync record). It is deliberately
destructive and does not prompt, so it only ever acts on the paths you name —
a bare `rm` with no path is refused. To delete a whole directory subtree, run
`ferry rm --recursive <dir>` from a terminal.

## Claude Code / Codex hook

For LLM agents (Claude Code, Codex, etc.) that read and edit files on your
behalf, register `ferry hook` as a `PreToolUse` hook so every Read/Edit
tool call auto-pulls the file from FTP first. There's a configurable
cooldown (default 3600s) so a hot LLM turn doesn't hammer the server.

Example Claude Code `~/.claude/settings.json` (or project-local
`.claude/settings.local.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Edit|MultiEdit|Write",
        "hooks": [{"type": "command", "command": "ferry hook"}]
      },
      {
        "matcher": "Read",
        "hooks": [{"type": "command", "command": "ferry hook --cooldown 3600"}]
      }
    ]
  }
}
```

See [`examples/claude-code-settings.json`](examples/claude-code-settings.json)
for a copy-pasteable version. The hook exits 0 whether it pulled, skipped
by cooldown, or errored — the LLM's tool call is never blocked.

Behaviour:
- Reads the tool envelope on stdin; extracts `tool_input.file_path`.
- Walks upward to find `.ferry.toml`; if not in a ferry project, no-op.
- Compares `state.files[rel].last_synced` against the cooldown window; if
  fresh enough, skips the pull.
- Otherwise runs a fast single-file pull (bypasses the tree walk).
- On failure, logs to stderr (which the LLM host surfaces to you) but
  never denies the tool call.

## Auto-pull on open (Zed extension)

For a hands-off workflow where opening a file automatically pulls the current
remote version, install the companion extension at
[`extensions/ferry/`](extensions/ferry/README.md). It attaches a minimal
language server to `.c`/`.h` files that shells out to `ferry pull --force`
on `textDocument/didOpen`. Install with:

```sh
cargo install --path .          # installs both ferry and ferry-lsp
cd extensions/ferry
zed --dev-extension .            # or use the Extensions palette
```

See the extension's README for behaviour details and the "brief flash of
stale content" caveat inherent to the approach.

## Remote compile checks (`cc`)

For servers that run the companion **UDP compile service** — an authenticated
`check_compile` endpoint (as used by the 3Kingdoms/3Scapes LDMud MUD) — `ferry
cc` dry-compiles files on the server without loading them:

```sh
ferry cc <file>...
# e.g.
ferry cc cmds/secure/cc.c players/foo/room.c
```

For each file it prints `<path>: OK` or `<path>: FAIL` followed by any
compiler diagnostics (`<file>:<line>: error|warning: <message>`), and exits
non-zero if any file failed — so it works as a pre-push compile gate. `check`
is an alias: `ferry check <file>...`.

Under the hood each file is one authenticated request (your `.ferry.toml`
login/password) over a small tab-delimited UDP protocol; the server streams
back chunked diagnostics that the client reassembles. Nothing is loaded or
executed on the server — it's a pure compile check, and you can only check
files you're allowed to write.

### Configuration

`cc` connects to the server's UDP port, set under `[connection]` in
`.ferry.toml` (defaults to `3203`):

```toml
[connection]
host     = "your.mud.host"
port     = 3201          # FTP/TCP port
udp_port = 3203          # compile-service UDP port
user     = "yourwiz"
password = "..."
```

This feature requires the server-side compile service to be installed; against
a plain FTP server `cc` simply times out.

## Security

**`.ferry.toml` stores your FTP password in plaintext.** `init` automatically
appends the config filename to `.gitignore`, but the file is still readable by
anything on your machine that can read your working tree. FTPS and SFTP are
explicitly out of scope for v1 — if you need encrypted transport, use a
different tool.

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Success |
| 1    | Generic error (I/O, unexpected failure) |
| 2    | Conflict — local and remote diverge; re-run with `--force` to override |
| 3    | Configuration or authentication problem |

These are stable so you can branch on them from shell scripts or task runners.

## Troubleshooting

- **"MDTM not supported"** — some FTP servers don't implement `MDTM`, which
  ferry uses to skip re-hashing unchanged remote files. It falls back to
  always downloading and re-hashing. The decision is cached in `state.json`
  after the first run so the probe only happens once.
- **"Connection refused"** — check the port (default `21`) and your server's
  passive-mode setting. ferry uses passive mode.
- **"Conflict: ..."** — run `ferry status` to see which files diverge, then
  either `pull`/`push` the side you want to keep, or re-run with `--force` to
  blow away the other side.

## Development

```sh
cargo test                  # unit + non-Docker integration tests
cargo test -- --ignored     # Docker-gated FTP integration tests
```

The `--ignored` suite requires a working Docker daemon and pulls
`delfer/alpine-ftp-server` to spin up a real FTP server per test.

## License

[PolyForm Noncommercial 1.0.0](LICENSE.md) — free to use, modify, and share
for any noncommercial purpose. Commercial use requires a separate license
from the copyright holder; get in touch if you want one.
