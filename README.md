# zed-ftp

A small Rust CLI that gives [Zed](https://zed.dev) editor users an FTP sync
workflow — `push`, `pull`, `sync`, `status`, and `init` — designed to be
triggered from `.zed/tasks.json` so you can map them to keybindings or the
command palette.

## Status

Functional and unit-tested. The FTP integration tests are gated behind a live
Docker daemon (they spin up a real `vsftpd` container), and the end-to-end
smoke test of the Zed `tasks.json` flow has **not yet been run** in this
environment because Docker was unavailable. Expect to verify the round-trip
against your own server before relying on it.

## Installation

From a checkout of this repo:

```sh
cargo install --path .
```

This drops a `zed-ftp` binary into `~/.cargo/bin`.

## Quick start

In your project root:

```sh
zed-ftp init
```

The wizard prompts for host, username, password, and remote root, then
validates by listing the remote root and walking it against your local tree.
Use `--no-validate` to skip the remote walk if you just want the config file
written:

```sh
zed-ftp init --no-validate
```

This writes a `.zed-ftp.toml` to the project root and appends it to
`.gitignore`.

## Tasks.json integration

Copy [`examples/tasks.json`](examples/tasks.json) into your project's
`.zed/tasks.json` (or merge the entries with your existing tasks). Then in
Zed, open the command palette and run `task: spawn` to pick one of:

- `FTP: push current file`
- `FTP: pull current file`
- `FTP: status`
- `FTP: sync all`

The per-file tasks use Zed's `$ZED_RELATIVE_FILE` variable so they operate on
whichever buffer is active.

## Claude Code / Codex hook

For LLM agents (Claude Code, Codex, etc.) that read and edit files on your
behalf, register `zed-ftp hook` as a `PreToolUse` hook so every Read/Edit
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
        "hooks": [{"type": "command", "command": "zed-ftp hook"}]
      },
      {
        "matcher": "Read",
        "hooks": [{"type": "command", "command": "zed-ftp hook --cooldown 3600"}]
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
- Walks upward to find `.zed-ftp.toml`; if not in a zed-ftp project, no-op.
- Compares `state.files[rel].last_synced` against the cooldown window; if
  fresh enough, skips the pull.
- Otherwise runs a fast single-file pull (bypasses the tree walk).
- On failure, logs to stderr (which the LLM host surfaces to you) but
  never denies the tool call.

## Auto-pull on open (Zed extension)

For a hands-off workflow where opening a file automatically pulls the current
remote version, install the companion extension at
[`extensions/zed-ftp/`](extensions/zed-ftp/README.md). It attaches a minimal
language server to `.c`/`.h` files that shells out to `zed-ftp pull --force`
on `textDocument/didOpen`. Install with:

```sh
cargo install --path .          # installs both zed-ftp and zed-ftp-lsp
cd extensions/zed-ftp
zed --dev-extension .            # or use the Extensions palette
```

See the extension's README for behaviour details and the "brief flash of
stale content" caveat inherent to the approach.

## Security

**`.zed-ftp.toml` stores your FTP password in plaintext.** `init` automatically
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
  zed-ftp uses to skip re-hashing unchanged remote files. It falls back to
  always downloading and re-hashing. The decision is cached in `state.json`
  after the first run so the probe only happens once.
- **"Connection refused"** — check the port (default `21`) and your server's
  passive-mode setting. zed-ftp uses passive mode.
- **"Conflict: ..."** — run `zed-ftp status` to see which files diverge, then
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

MIT (placeholder — update before publishing).
