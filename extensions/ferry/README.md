# Ferry extension

Ferry's Zed extension starts the native `ferry-lsp` language server for C
files. The server handles `textDocument/didOpen` and `textDocument/didSave`
events and exposes five manual file actions. It does not provide completion,
hover, or language diagnostics.

## Prerequisites

1. Install the `ferry` and `ferry-lsp` binaries from the repository root:

   ```sh
   cargo install --path .
   ```

   This installs both binaries in `~/.cargo/bin`; make sure that directory is
   on `PATH`. Native Compare also requires the `zed` CLI to be on the `PATH`
   visible to `ferry-lsp`.

2. Configure a project by running `ferry init` at its root. For a file inside
   nested Ferry projects, the nearest `.ferry.toml` above that file wins. A
   file outside every Ferry project is ignored.

## Install the extension in Zed

Open Zed's Extensions page and click `Install Dev Extension`, or run the
`zed: install dev extension` action. Select this `extensions/ferry` directory.

## Configuration

The `[editor]` settings and their defaults are:

```toml
[editor]
pull_on_open = false
push_on_save = false
```

Both settings default to `false`. Each project can opt into Pull on open,
Push on save, or both independently. Ferry reads the nearest project
configuration again on every open, save, and manual action, so a settings
change takes effect on the next event without restarting Zed.

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

Automatic settings do not hide manual actions. Open Zed's lightbulb menu or
press `Ctrl-.` on a file in a Ferry project to choose, in order:

1. `Ferry: Pull`
2. `Ferry: Compare with Remote`
3. `Ferry: Force Pull (overwrite local)`
4. `Ferry: Push`
5. `Ferry: Compile-check`

Pull, Compare, and Force Pull are save-first operations: they use the saved
local file. If the Zed buffer has unsaved changes, Ferry refuses the action and
asks you to save and retry. Compare fetches the remote file into a private
snapshot, then opens Zed's native diff with the saved local file on the left
(old) and the fetched remote file on the right (new). It does not change the
local file, Ferry state, or sync settings.

Force Pull retrieves the remote file first, then displays Zed's native warning
confirmation. Only the exact `Overwrite local file` action applies it. Cancel,
dismissal, an edit, shutdown, or a change to the local file's identity leaves
the current file and Ferry state intact. A confirmed overwrite updates the
local file and state through a guarded atomic install. This confirmation is
specific to the Zed action; the existing `ferry pull --force` CLI remains
noninteractive and unchanged.

Manual Pull and Push remain non-force. All five actions stay scoped to the
current file's nearest Ferry project. Manual actions report success with an
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
