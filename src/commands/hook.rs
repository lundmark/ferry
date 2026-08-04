//! `ferry hook` — Claude Code / Codex PreToolUse hook.
//!
//! Reads a hook-envelope JSON object from stdin, extracts the file path
//! from the tool's inputs, and — if the file lives under a ferry
//! project — pulls it (with a configurable cooldown). Always exits 0
//! so the LLM's tool call is never blocked; failures are surfaced on
//! stderr, which the hosting agent shows to the user.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::commands::ExecutionMode;
use crate::config::Config;
use crate::state::StateFile;

/// Shape shared by Claude Code (snake_case) hook input. Codex uses a
/// slightly different envelope but the same `tool_name` / `tool_input`
/// nesting works. Missing fields are tolerated.
#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

pub fn run(cooldown_secs: i64, mode: ExecutionMode) -> Result<()> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading hook envelope on stdin")?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let input: HookInput = serde_json::from_str(&buf)
        .with_context(|| format!("parsing hook envelope: {buf}"))?;

    // Only some tools carry a file_path. Bash/Grep/Glob don't; allow them
    // through without any FTP work.
    let tool_input = match input.tool_input {
        Some(v) => v,
        None => return Ok(()),
    };
    let file_path = tool_input
        .get("file_path")
        .and_then(|v| v.as_str())
        .or_else(|| tool_input.get("path").and_then(|v| v.as_str()));
    let file_path = match file_path {
        Some(p) => Path::new(p),
        None => return Ok(()),
    };
    if !file_path.is_absolute() {
        // Claude Code sends absolute paths; anything else is likely a
        // shape we don't recognize — bail out silently rather than guess.
        return Ok(());
    }

    // Walk up to find a project root (holding the config file, under either the
    // current or the legacy name). If none, we're not in a ferry project; the
    // tool call has nothing to do with FTP.
    let root = match find_project_dir_upward(file_path) {
        Some(p) => p,
        None => return Ok(()),
    };
    // Best-effort one-time rename of legacy .zed-ftp files. Never fail the hook
    // over it — a rename that can't happen (e.g. read-only FS) just means we
    // fall back to reading the legacy names below.
    if mode.should_apply() {
        if let Err(e) = crate::names::migrate_legacy(&root) {
            eprintln!("ferry hook: migration warning: {e:#}");
        }
    }
    // Prefer the current names; tolerate legacy ones if migration couldn't run
    // so the hook keeps working rather than silently going dark.
    let config_path = existing_or(&root, crate::names::CONFIG_FILE, crate::names::LEGACY_CONFIG_FILE);
    let state_dir = existing_or(&root, crate::names::STATE_DIR, crate::names::LEGACY_STATE_DIR);

    let rel = match file_path.strip_prefix(&root) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => return Ok(()),
    };

    // Cooldown check. If the state entry's last_synced is within the
    // cooldown window, skip the pull.
    // (Loading Config is deferred until we know we're going to act, so a
    // malformed config doesn't kill unrelated tool calls.)
    let state_path = state_dir.join("state.json");
    if let Ok(state) = StateFile::load_or_default(&state_path) {
        if let Some(record) = state.files.get(&rel) {
            let elapsed = Utc::now().signed_duration_since(record.last_synced);
            if elapsed.num_seconds() >= 0 && elapsed.num_seconds() < cooldown_secs {
                let tool = input.tool_name.as_deref().unwrap_or("<unknown>");
                eprintln!(
                    "ferry hook: {tool} {rel} — within {cooldown_secs}s cooldown, skipping pull"
                );
                return Ok(());
            }
        }
    }

    // Load config lazily now that we know we're about to act.
    let _cfg_check = Config::load(&config_path)?;

    // Fast single-file pull with force=true. The hook contract is "give me
    // the current remote version"; users who don't want that shouldn't
    // install the hook.
    match crate::commands::pull::pull_one(&config_path, &rel, /* force = */ true, mode) {
        Ok(true) if mode.is_dry_run() => eprintln!("ferry hook: would pull {rel}"),
        Ok(true) => eprintln!("ferry hook: pulled {rel}"),
        Ok(false) => {
            // Already in sync (or local-only). No output needed.
        }
        Err(e) => {
            // Don't fail the hook — deny would surprise the user. Just log.
            eprintln!("ferry hook: pull {rel} failed: {e:#}");
        }
    }
    Ok(())
}

/// Walk up from `start` to the nearest ancestor directory that holds the config
/// file under either the current (`.ferry.toml`) or legacy (`.zed-ftp.toml`)
/// name. Matching both lets the hook detect — and then migrate — a project that
/// predates the rename.
fn find_project_dir_upward(start: &Path) -> Option<PathBuf> {
    use crate::names::{CONFIG_FILE, LEGACY_CONFIG_FILE};
    let mut current = start.parent()?;
    loop {
        if current.join(CONFIG_FILE).exists() || current.join(LEGACY_CONFIG_FILE).exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// Return `dir/preferred` if it exists, otherwise `dir/fallback`. Used to read
/// through to legacy names when a migration couldn't be performed.
fn existing_or(dir: &Path, preferred: &str, fallback: &str) -> PathBuf {
    let p = dir.join(preferred);
    if p.exists() {
        p
    } else {
        dir.join(fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_input_parses_claude_code_shape() {
        let json = r#"{
            "session_id": "abc",
            "tool_name": "Read",
            "tool_input": {"file_path": "/tmp/foo.c"}
        }"#;
        let input: HookInput = serde_json::from_str(json).unwrap();
        assert_eq!(input.tool_name.as_deref(), Some("Read"));
        assert_eq!(
            input.tool_input.unwrap().get("file_path").and_then(|v| v.as_str()),
            Some("/tmp/foo.c")
        );
    }

    #[test]
    fn hook_input_tolerates_missing_fields() {
        let input: HookInput = serde_json::from_str("{}").unwrap();
        assert!(input.tool_name.is_none());
        assert!(input.tool_input.is_none());
    }

    #[test]
    fn find_project_dir_finds_nested_new_name() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(dir.path().join(crate::names::CONFIG_FILE), "").unwrap();
        let file = deep.join("x.c");
        std::fs::write(&file, "").unwrap();
        let found = find_project_dir_upward(&file).unwrap();
        assert_eq!(found, dir.path());
    }

    #[test]
    fn find_project_dir_finds_legacy_name() {
        // A project that predates the rename must still be discovered so it can
        // be migrated.
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(dir.path().join(crate::names::LEGACY_CONFIG_FILE), "").unwrap();
        let file = deep.join("x.c");
        std::fs::write(&file, "").unwrap();
        let found = find_project_dir_upward(&file).unwrap();
        assert_eq!(found, dir.path());
    }

    #[test]
    fn find_project_dir_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.c");
        std::fs::write(&file, "").unwrap();
        assert!(find_project_dir_upward(&file).is_none());
    }
}
