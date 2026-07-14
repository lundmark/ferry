# Ferry extension

Attaches a minimal language server to C files that shells out to
`ferry pull` whenever a file is opened. The LSP is a no-op otherwise —
no completions, no diagnostics, no hover; its only job is to trigger a
pull on `textDocument/didOpen`.

## Prerequisites

1. Install the `ferry` CLI and the `ferry-lsp` binary. From the
   repo root:

   ```
   cargo install --path .
   ```

   That produces both `ferry` and `ferry-lsp` in `~/.cargo/bin`.
   Make sure that directory is on your PATH.

2. Configure a project by running `ferry init` at the project root.
   The extension walks up from any opened file to find the nearest
   `.ferry.toml`; if none is found, the LSP silently no-ops.

## Install the extension in Zed

From this directory:

```
zed --dev-extension .
```

Or use `Extensions: Install Dev Extension` from Zed's command palette
and point it at this folder.

## Behaviour

- On opening a `.c` or `.h` file anywhere under a project with a
  `.ferry.toml`, the LSP calls `ferry pull <file> --force`.
- `--force` is deliberate: the LSP scenario is "give me the current
  remote version." If you have locally-modified files you don't want
  overwritten, don't install this extension.
- The pull is synchronous inside the LSP loop but non-blocking to
  Zed's UI. You'll briefly see the stale on-disk content in the
  editor buffer while the pull runs; Zed's external-file-change
  watcher then reloads the buffer with fresh content.
- On pull failure the LSP sends a `window/showMessage` warning.

## Caveats

- Only `.c`/`.h` are attached by default. Add more languages under
  `[language_servers.ferry-lsp].languages` in `extension.toml`.
- If the pull refuses (e.g. state divergence the design defines as a
  conflict), you'll see a warning notification but the editor still
  opens the local (stale) file.
