# Ferry extension

Ferry's Zed extension starts the native `ferry-lsp` language server for C
files. The server handles `textDocument/didOpen` and `textDocument/didSave`
events and exposes seven manual file actions. It does not provide completion,
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
- No automatic event performs a whole-tree or directory sync; directory sync
  is always an explicit manual operation.
- Zed opens the existing on-disk content before the Pull completes. The buffer
  can therefore briefly show stale content before Zed observes the external
  file change.

The language server performs transfer work away from the protocol loop, so
other LSP messages continue to be handled while an FTP or compile request is
in progress.

## Manual actions and tasks

Zed's extension API does not let Ferry add literal Project Panel right-click
actions. Use the current document's Code Action menu or the terminal-backed
Task Picker instead.

Automatic settings do not hide manual actions. Open Zed's lightbulb menu or
press `Ctrl-.` on a file in a Ferry project to choose, in order:

1. `Ferry: Pull`
2. `Ferry: Compare with Remote`
3. `Ferry: Force Pull (overwrite local)`
4. `Ferry: Push`
5. `Ferry: Sync Current File`
6. `Ferry: Sync Current Folder`
7. `Ferry: Compile-check`

Sync Current File reconciles only the saved current file. Sync Current Folder
recursively reconciles the current file's parent directory. Pull, Compare,
Force Pull, and both Sync actions are save-first: Ferry refuses a file action
when its buffer is dirty, and refuses folder sync when a known buffer beneath
that folder is dirty. Save the file or use Save All, then retry.

Compare fetches the remote file into a private snapshot, then opens Zed's
native diff with the saved local file on the left (old) and the fetched remote
file on the right (new). It does not change the local file, Ferry state, or
sync settings.

Force Pull retrieves the remote file first, then displays Zed's native warning
confirmation. Only the exact `Overwrite local file` action applies it. Cancel,
dismissal, an edit, shutdown, or a change to the local file's identity leaves
the current file and Ferry state intact. A confirmed overwrite updates the
local file and state through a guarded atomic install. This confirmation is
specific to the Zed action; the existing `ferry pull --force` CLI remains
noninteractive and unchanged.

Manual Pull, Push, and both Sync actions remain non-force. All seven actions
stay scoped to the current file's nearest Ferry project. Manual actions report
success with an Info notification and conflicts or failures with a Warning
notification.

For terminal output, recursive folder sync, interactive selection, or
project-wide Status/Sync, copy
[`../../examples/tasks.json`](../../examples/tasks.json) into the project's
`.zed/tasks.json` and use Zed's Task Picker. It includes these scoped tasks:

- `Ferry: sync current file`
- `Ferry: sync current folder`
- `Ferry: choose path to sync...`

Run **Save All** before a terminal-backed directory or picker task. Terminal
tasks cannot inspect dirty Zed buffers. The chooser starts in the active
file's directory, can browse local and remote entries including remote-only
roots, synchronizes one selected file or folder, and then closes. Supplied
tasks never use force or deletion.

Projects can add a stable named task for a frequently used folder, such as
`ferry sync areas`:

```json
{
  "label": "Ferry: sync areas",
  "command": "ferry",
  "args": ["sync", "areas"],
  "cwd": "$ZED_WORKTREE_ROOT",
  "use_new_terminal": false,
  "reveal": "always",
  "hide": "on_success"
}
```

### Scoped CLI behavior

Bare `ferry sync` retains the established project-wide behavior. `ferry sync
PATH` accepts exactly one file or directory; a directory is recursive.
`ferry sync --select` opens the single-selection picker, and `PATH` and
`--select` are mutually exclusive.

`ferry sync --force` is an explicit CLI-only local-wins option for
both-changed and untracked files. It is not exposed by either Sync Code Action
or any supplied task. Global `--dry-run` is non-mutating and previews direct
or selected scoped work without changing local files, remote files or
directories, or Ferry state.

Scoped sync creates remote-only directories locally, local-only directories
remotely, and empty selected directories and descendants on the missing side.
It never propagates deletion: a file or directory that exists on one side is
not removed. Clean entries may finish before another entry reports a conflict;
that partial clean progress is retained, all conflicts are reported, and the
command returns conflict exit code `2`.

## Language attachment caveat

The extension attaches to Zed's built-in C language only. Zed normally treats
`.h` files as C, so they are usually covered. If a header is classified as C++
or another language in your setup, Ferry will not attach to it unless you add
that language under `[language_servers.ferry-lsp].languages` in
`extension.toml`.
