# Zed Code-Actions Keybinding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore `Ctrl+.` as Zed's code-action shortcut while preserving the user's SublimeText base keymap and all unrelated custom bindings.

**Architecture:** Install one editor-scoped override in the user's Flatpak Zed `keymap.json`. Treat the live file as mutable external state: prove the desired binding is absent, recheck immediately before writing, create it only when absent, and merge without replacement if it exists.

**Tech Stack:** Zed JSON keymap configuration, Flatpak user configuration, `jq`, shell read-only checks

---

### Task 1: Capture the failing keybinding state

**Files:**
- Inspect: `/home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json`
- Reference: `/home/simon/.var/app/dev.zed.Zed/config/zed/settings.json`

- [ ] **Step 1: Confirm the selected base keymap**

Run:

```bash
rg -n '^[[:space:]]*"base_keymap"[[:space:]]*:[[:space:]]*"SublimeText"[[:space:]]*,?[[:space:]]*$' /home/simon/.var/app/dev.zed.Zed/config/zed/settings.json
```

Expected: exit 0 and exactly one matching line containing the SublimeText property.

- [ ] **Step 2: Run the failing binding check**

Run:

```bash
test -f /home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json && jq -e 'any(.[]; .context == "Editor" and .bindings["ctrl-."] == "editor::ToggleCodeActions")' /home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json
```

Expected before implementation: nonzero exit because the custom keymap is absent or does not contain the approved binding.

- [ ] **Step 3: Confirm Ferry is already healthy**

Run:

```bash
ps -eo args | rg '^/home/simon/.cargo/bin/ferry-lsp$'
```

Expected: one running Ferry language-server process. Do not invoke any Ferry command.

### Task 2: Install the minimal custom keymap

**Files:**
- Create or merge: `/home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json`
- Temporary staging file: `/home/simon/code/3s/.tmp-zed-keymap.json`

- [ ] **Step 1: Recheck the live target immediately before editing**

Run:

```bash
if test -e /home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json; then sed -n '1,260p' /home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json; else printf '%s\n' 'keymap.json is absent'; fi
```

Expected in the observed state: `keymap.json is absent`. If it exists, do not replace it: use `apply_patch` to add only the approved `ctrl-.` binding to an existing `Editor` context or append a new `Editor` context while preserving every unrelated entry.

- [ ] **Step 2: Create the staged keymap with the patch helper**

Create `/home/simon/code/3s/.tmp-zed-keymap.json` with exactly:

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

- [ ] **Step 3: Validate the staged keymap**

Run:

```bash
jq -e 'length == 1 and .[0].context == "Editor" and .[0].bindings["ctrl-."] == "editor::ToggleCodeActions"' /home/simon/code/3s/.tmp-zed-keymap.json
```

Expected: exit 0 and output `true`.

- [ ] **Step 4: Install or merge without data loss**

If the target remains absent, copy the staged file to `/home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json` with mode `0644`. If it now exists, stop the copy and merge only the approved binding with `apply_patch`. Never overwrite an existing file.

- [ ] **Step 5: Remove the staging file**

Delete only `/home/simon/code/3s/.tmp-zed-keymap.json` after successful installation.

### Task 3: Verify Zed accepted the override

**Files:**
- Verify: `/home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json`
- Inspect: `/home/simon/.var/app/dev.zed.Zed/data/zed/logs/Zed.log`

- [ ] **Step 1: Run the binding check again**

Run:

```bash
jq -e 'any(.[]; .context == "Editor" and .bindings["ctrl-."] == "editor::ToggleCodeActions")' /home/simon/.var/app/dev.zed.Zed/config/zed/keymap.json
```

Expected: exit 0 and output `true`.

- [ ] **Step 2: Check Zed's log for reload errors**

Inspect only new log entries containing `keymap`, `binding`, or parse errors after the file write.

Expected: no error referring to `keymap.json`, `ctrl-.`, or `editor::ToggleCodeActions`.

- [ ] **Step 3: Confirm Ferry remains alive**

Run:

```bash
ps -eo args | rg '^/home/simon/.cargo/bin/ferry-lsp$'
```

Expected: one running process.

- [ ] **Step 4: Perform the safe UI check**

With a C file inside `/home/simon/code/3s` focused, press `Ctrl+.` and confirm the menu contains `Ferry: Pull`, `Ferry: Push`, and `Ferry: Compile-check`. Close the menu without selecting an action so no local or remote Ferry operation runs.

- [ ] **Step 5: Record completion**

No implementation commit is expected because `keymap.json` is personal configuration outside the Ferry repository. Record the exact validation results and leave the committed design and plan documents on durable `main`.
