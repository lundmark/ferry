# Zed Installation Discoverability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Ferry's Zed development-extension installation immediately discoverable and accurate in both READMEs.

**Architecture:** Treat the root README's top-level installation section as the canonical short workflow, then link to its existing Native Zed integration section for configuration and actions. Keep the extension README self-contained while matching the canonical prerequisites and troubleshooting language.

**Tech Stack:** Markdown, ripgrep/shell documentation assertions, Git

---

## File Map

- Modify: `README.md` — canonical binary and Zed installation workflow; remove duplicated installation details from the later integration section.
- Modify: `extensions/ferry/README.md` — self-contained Zed extension prerequisites, installation, and conditional troubleshooting.

### Task 1: Make the root installation section canonical

**Files:**
- Modify: `README.md:22-32`
- Modify: `README.md:195-211`

- [ ] **Step 1: Run the discoverability checks and verify the current README fails**

Run:

```sh
sed -n '20,70p' README.md | rg -n 'ferry-lsp|Install Dev Extension|rustup'
```

Expected: no output and exit status 1, proving the top installation section omits all three details.

Run:

```sh
rg -nF 'This drops a `ferry` binary into `~/.cargo/bin`.' README.md
```

Expected: one match, proving the installed-binary description is stale.

- [ ] **Step 2: Replace the root installation text**

Replace the current sentence after `cargo install --path .` with:

```markdown
This installs both `ferry` and `ferry-lsp` in `~/.cargo/bin`. Make sure
that directory is on `PATH`.

### Install in Zed

Zed requires Rust to be installed through
[`rustup`](https://zed.dev/docs/extensions/developing-extensions) when
building a development extension.

Open Zed's Extensions page and click `Install Dev Extension`, or run the
`zed: install dev extension` action. Select this repository's
`extensions/ferry` directory.

After configuring a Ferry project, if its actions do not appear for a file,
fully quit and relaunch Zed, then reopen the project.

See [Native Zed integration](#native-zed-integration) for project configuration
and the available actions.
```

- [ ] **Step 3: Remove the duplicate installation flow from Native Zed integration**

Keep the opening description of the companion extension, then replace the
duplicate Cargo command and `Install Dev Extension` paragraphs with:

```markdown
Follow [Installation](#installation) to install both binaries and the
development extension in Zed.
```

Leave the existing project configuration and action documentation unchanged.

- [ ] **Step 4: Run the root README checks**

Run:

```sh
sed -n '20,80p' README.md | rg -n 'ferry-lsp|Install Dev Extension|rustup'
test "$(rg -c '^cargo install --path \.$' README.md)" -eq 1
test "$(rg -c 'zed: install dev extension' README.md)" -eq 1
! rg -nF 'This drops a `ferry` binary into `~/.cargo/bin`.' README.md
rg -n '^## Native Zed integration$' README.md
```

Expected: the first command finds all three details; each remaining command
exits 0; the Cargo and Zed installation instructions each have one canonical
root-README occurrence.

- [ ] **Step 5: Commit the root README change**

```sh
git add README.md
git commit -m "docs: surface Zed development extension install"
```

### Task 2: Align the extension README

**Files:**
- Modify: `extensions/ferry/README.md:7-29`

- [ ] **Step 1: Verify the extension README lacks the new guidance**

Run:

```sh
rg -n 'rustup|fully quit and relaunch Zed' extensions/ferry/README.md
```

Expected: no output and exit status 1.

- [ ] **Step 2: Add the Zed prerequisite**

Insert this as the first numbered prerequisite and renumber the existing two
items:

```markdown
1. Install Rust through
   [`rustup`](https://zed.dev/docs/extensions/developing-extensions). Zed
   requires a `rustup` toolchain to build development extensions.
```

Keep the binary-installation and `ferry init` prerequisites otherwise
unchanged.

- [ ] **Step 3: Add conditional troubleshooting**

After the `Install Dev Extension` selection paragraph, add:

```markdown
If Ferry actions do not appear for a file in a configured Ferry project, fully
quit and relaunch Zed, then reopen the project.
```

- [ ] **Step 4: Run consistency and formatting checks**

Run:

```sh
rg -nF 'https://zed.dev/docs/extensions/developing-extensions' README.md
rg -nF 'https://zed.dev/docs/extensions/developing-extensions' extensions/ferry/README.md
rg -nF 'relaunch Zed, then reopen the project.' README.md
rg -nF 'relaunch Zed, then reopen the project.' extensions/ferry/README.md
test -f extensions/ferry/README.md
rg -nF '[Native Zed integration](#native-zed-integration)' README.md
rg -nF '[Installation](#installation)' README.md
git diff --check
```

Expected: every command exits 0; both READMEs contain the same official
prerequisite link and conditional relaunch guidance; no whitespace errors are
reported.

- [ ] **Step 5: Review the complete documentation diff**

Run:

```sh
git diff -- README.md extensions/ferry/README.md
```

Expected: only the approved installation, cross-link, prerequisite, and
troubleshooting text changes; no behavior or configuration documentation
changes.

- [ ] **Step 6: Commit the extension README change**

```sh
git add extensions/ferry/README.md
git commit -m "docs: align Ferry extension setup guidance"
```

### Task 3: Verify the finished documentation branch

**Files:**
- Verify: `README.md`
- Verify: `extensions/ferry/README.md`

- [ ] **Step 1: Run all targeted documentation assertions against committed files**

Run:

```sh
sed -n '20,80p' README.md | rg -n 'ferry-lsp|Install Dev Extension|rustup'
test "$(rg -c '^cargo install --path \.$' README.md)" -eq 1
test "$(rg -c 'zed: install dev extension' README.md)" -eq 1
rg -nF 'https://zed.dev/docs/extensions/developing-extensions' README.md
rg -nF 'https://zed.dev/docs/extensions/developing-extensions' extensions/ferry/README.md
rg -nF 'relaunch Zed, then reopen the project.' README.md
rg -nF 'relaunch Zed, then reopen the project.' extensions/ferry/README.md
! rg -nF 'This drops a `ferry` binary into `~/.cargo/bin`.' README.md
pandoc --from=gfm --to=html README.md -o /tmp/ferry-readme.html
pandoc --from=gfm --to=html extensions/ferry/README.md -o /tmp/ferry-extension-readme.html
rg -nF 'id="installation"' /tmp/ferry-readme.html
rg -nF 'href="#native-zed-integration"' /tmp/ferry-readme.html
rg -nF 'id="install-the-extension-in-zed"' /tmp/ferry-extension-readme.html
git diff --check main...HEAD
git status --short
```

Expected: every assertion exits 0; Pandoc renders both READMEs and exposes the
expected heading IDs and internal link; `git diff --check` prints nothing; and
`git status --short` prints nothing.

- [ ] **Step 2: Confirm no production files changed**

Run:

```sh
git diff --name-only main...HEAD
```

Expected output consists only of:

```text
README.md
docs/superpowers/plans/2026-08-11-zed-install-discoverability.md
docs/superpowers/specs/2026-08-11-zed-install-discoverability-design.md
extensions/ferry/README.md
```
