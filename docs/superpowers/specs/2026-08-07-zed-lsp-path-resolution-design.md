# Zed LSP Path Resolution Design

## Problem

The Ferry development extension returns the bare command name `ferry-lsp` from
`language_server_command`. Zed interprets that value as an extension-work-dir
relative path, so it tries to launch `extensions/work/ferry/ferry-lsp` instead
of the executable installed on the worktree shell's `PATH`. The launch fails
before the language server can advertise Ferry's code actions.

## Decision

Keep the existing installation contract: users install `ferry` and
`ferry-lsp` with `cargo install --path .`, and the extension finds
`ferry-lsp` on the project worktree's shell `PATH`.

The extension will call `Worktree::which("ferry-lsp")` and put the returned
absolute path in Zed's `Command`. If lookup fails, it will return an actionable
error that tells the user to install Ferry and ensure it is on `PATH`.

## Alternatives considered

- Bundle a native executable with the extension. This would require builds and
  distribution for every supported platform and duplicates Cargo installation.
- Add a configurable executable path. This adds settings and validation for a
  problem Zed's worktree-aware lookup already solves.

## Testing and verification

Extract command construction behind a small function that accepts the lookup
result. Unit tests will prove that an absolute discovered path is preserved and
that a missing binary produces the installation hint. The production method
will pass `worktree.which("ferry-lsp")` into that function; compiling the
extension verifies the Zed API integration.

After the automated suite passes, rebuild/reload the installed development
extension and confirm Zed's log reports `ferry-lsp` starting from the installed
Cargo path rather than its extension work directory. Perform that check in a
temporary Ferry project with dummy connection values and this editor configuration:

```toml
[editor]
pull_on_open = false
push_on_save = false
```

This prevents opening or saving the test C file from contacting a live server.
