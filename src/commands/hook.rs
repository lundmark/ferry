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
use std::path::Path;

use crate::commands::{state_path_for, ExecutionMode};
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
    if let Err(error) = std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading hook envelope on stdin")
    {
        eprintln!("ferry hook: {error:#}");
        return Ok(());
    }
    if buf.trim().is_empty() {
        return Ok(());
    }
    let input: HookInput = match serde_json::from_str(&buf)
        .with_context(|| format!("parsing hook envelope: {buf}"))
    {
        Ok(input) => input,
        Err(error) => {
            eprintln!("ferry hook: {error:#}");
            return Ok(());
        }
    };

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

    let resolved = match crate::project::resolve_file(file_path, mode.should_apply()) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return Ok(()),
        Err(error) => {
            eprintln!("ferry hook: resolving {} failed: {error:#}", file_path.display());
            return Ok(());
        }
    };
    let config_path = crate::names::config_path_for_read(&resolved.config_dir);
    let state_path = state_path_for(&resolved.config.paths.local_root, mode);
    let rel = resolved.relative_path;

    // Cooldown check. If the state entry's last_synced is within the
    // cooldown window, skip the pull. Apply mode keeps config loading deferred
    // until we know we're going to act; dry-run loaded it above only to resolve
    // the configured local root for read-through state selection.
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
}
