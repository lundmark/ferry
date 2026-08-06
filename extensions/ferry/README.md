# Ferry extension

Ferry's Zed extension starts the native `ferry-lsp` language server for C
files. The server handles `textDocument/didOpen` and `textDocument/didSave`
events and exposes manual Pull, Push, and Compile-check file actions. It does
not provide completion, hover, or language diagnostics.

## Prerequisites

1. Install the `ferry` and `ferry-lsp` binaries from the repository root:

   ```sh
   cargo install --path .
   ```

   This installs both binaries in `~/.cargo/bin`; make sure that directory is
   on `PATH`.

2. Configure a project by running `ferry init` at its root. For a file inside
   nested Ferry projects, the nearest `.ferry.toml` above that file wins. A
   file outside every Ferry project is ignored.

## Install the extension in Zed

From this directory, run:

```sh
zed --dev-extension .
```

Alternatively, run `Extensions: Install Dev Extension` from Zed's command
palette and select this directory.

## Configuration

Add an `[editor]` table to the project's `.ferry.toml` when you want to
override the defaults:

```toml
[editor]
pull_on_open = true
push_on_save = false
```

Pull on open defaults to `true`; Push on save defaults to `false`. Ferry reads
the nearest project configuration again on every open, save, and manual action,
so a settings change takes effect on the next event without restarting Zed.

## Behavior

- Opening a file pulls that file when `pull_on_open` is enabled.
- Saving a file pushes that file only when `push_on_save` is enabled.
- Automatic Pull and Push are always non-force and conflict-safe. A conflict
  or other failure produces a Warning notification; automatic success is
  silent.
- No automatic event performs a whole-tree sync.
- Zed opens the existing on-disk content before the Pull completes. The buffer
  can therefore briefly show stale content before Zed observes the external
  file change.

The language server performs transfer work away from the protocol loop, so
other LSP messages continue to be handled while an FTP or compile request is
in progress.

## Manual actions and tasks

Open Zed's lightbulb menu or press `Ctrl-.` on a file in a Ferry project to
choose exactly one of:

- `Ferry: Pull`
- `Ferry: Push`
- `Ferry: Compile-check`

Manual Pull and Push are also non-force. Manual actions report success with an
Info notification and conflicts or failures with a Warning notification.

For terminal output or project-wide Status/Sync, copy
[`../../examples/tasks.json`](../../examples/tasks.json) into the project's
`.zed/tasks.json` and use Zed's Task Picker. The example also contains an
explicitly labelled destructive task that deletes the current file both
locally and remotely; read the warning in the root README before using it.

## Language attachment caveat

The extension attaches to Zed's built-in C language only. Zed normally treats
`.h` files as C, so they are usually covered. If a header is classified as C++
or another language in your setup, Ferry will not attach to it unless you add
that language under `[language_servers.ferry-lsp].languages` in
`extension.toml`.
