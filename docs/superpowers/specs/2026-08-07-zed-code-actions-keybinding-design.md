# Zed Code-Actions Keybinding Design

## Context

Ferry is loaded by Zed for C files in `/home/simon/code/3s`, and the restarted
Zed process successfully starts `/home/simon/.cargo/bin/ferry-lsp`. Ferry
advertises code actions and returns three commands for files resolved inside a
Ferry project: `Ferry: Pull`, `Ferry: Push`, and `Ferry: Compile-check`.

The user selected Zed's `SublimeText` base keymap. In the installed Zed version,
that keymap overrides the default Linux `ctrl-.` binding and maps it to
`editor::GoToHunk`. When no changed hunk is available, pressing the shortcut
appears to do nothing.

## Decision

At investigation time, the user's custom Zed keymap file does not exist at
`/home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json`. Recheck that path
immediately before installation. If it is still absent, create it with one
editor-scoped binding:

```json
[
  {
    "context": "Editor",
    "bindings": {
      "ctrl-.": "editor::ToggleCodeActions"
    }
  }
]
```

Zed loads the custom keymap after its base keymap, so this restores the normal
code-action shortcut without changing the rest of the SublimeText mappings.
If the file exists by installation time, merge this binding into it while
preserving every unrelated context and binding rather than replacing the file.
The accepted trade-off is that `ctrl-.` no longer invokes `editor::GoToHunk`;
that action remains available through Zed's command palette or a future custom
binding.

## Scope

- Add the custom keymap above when absent, or merge only the approved binding
  if the file exists at installation time.
- Do not change Zed's global settings or selected base keymap.
- Do not change Ferry source code, its extension manifest, or `.ferry.toml`.
- Do not perform a Ferry pull, push, compile check, or other network operation
  during verification.

## Validation

1. Confirm the pre-change failure: no custom keymap provides a `ctrl-.` code-
   action override while the SublimeText base keymap maps it to Go to Hunk.
2. Recheck the target path, install or merge the custom keymap without changing
   unrelated entries, and verify its JSON structure and exact binding.
3. Confirm Zed reloads the keymap without a parse or binding error.
4. In a C file inside `/home/simon/code/3s`, confirm `ctrl-.` opens the code-
   action menu and exposes `Ferry: Pull`, `Ferry: Push`, and
   `Ferry: Compile-check`.
5. Do not select an action during verification, avoiding changes to local or
   remote project data.

## Success Criteria

- The SublimeText base keymap remains selected.
- `ctrl-.` invokes `editor::ToggleCodeActions` in editor buffers.
- Ferry remains running for the 3S worktree.
- The three Ferry actions are available for C files governed by the project
  `.ferry.toml`.
