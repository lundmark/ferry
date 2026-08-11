# Zed Installation Discoverability Design

## Context

The root README documents Zed's `Install Dev Extension` workflow only in the
later **Native Zed integration** section. A reader following the top-level
**Installation** section can therefore miss the Zed setup entirely. That
section also says `cargo install --path .` installs only `ferry`, although the
command installs both `ferry` and `ferry-lsp`.

## Decision

Make the root README's **Installation** section the short, accurate entry point
for both command-line and Zed installation:

1. State that `cargo install --path .` installs both binaries into
   `~/.cargo/bin` and that the directory must be on `PATH`.
2. Add an **Install in Zed** subsection immediately afterward. State Zed's
   requirement that Rust be installed through `rustup`, and link to Zed's
   official development-extension instructions.
3. Tell users to
   open Zed's Extensions page and choose `Install Dev Extension`, or invoke
   `zed: install dev extension`, then select this repository's
   `extensions/ferry` directory.
4. Add focused troubleshooting guidance: if Ferry actions do not appear after
   installation, fully quit and relaunch Zed, then reopen the project.
5. Link to **Native Zed integration** for project configuration and available
   actions.

Keep the later integration section focused on behavior and configuration.
Replace its duplicate binary and extension installation instructions with a
short link back to **Installation**, preventing the two sections from drifting
apart. Keep the extension-specific README self-contained, but add the same
`rustup` prerequisite and troubleshooting guidance there for consistency.

## Scope

- Update only `README.md` and `extensions/ferry/README.md`.
- Do not change Ferry behavior, Zed actions, configuration defaults, extension
  metadata, or build output.
- Do not document publication through Zed's extension registry; the supported
  workflow remains development-extension installation from a checkout.

## Validation

1. Confirm the root installation section names both installed binaries and
   exposes the complete Zed development-extension flow before **Quick start**.
2. Confirm the official Zed prerequisite link, all relative links, and all
   Markdown anchors resolve.
3. Search for stale wording that claims Cargo installs only `ferry` or for
   contradictory Zed installation instructions.
4. Review the rendered Markdown structure for concise, non-duplicated guidance.

## Success Criteria

- A new reader can find and complete Zed installation from the root README's
  top-level installation section.
- The command-line installation result is described accurately.
- Detailed Zed configuration and action documentation remains easy to reach.
- The root and extension READMEs agree on the first-install workflow.
