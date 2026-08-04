//! `ferry rm` — delete files on the remote server, the local mirror, and the
//! state record, in one deliberate command.
//!
//! Unlike `push`/`pull`/`sync`, `rm` is destructive by intent: it performs no
//! conflict check and prompts for nothing. Its guards are structural — at least
//! one explicit path is required (there is no "delete everything" mode), paths
//! may not escape the sync roots, and deleting a directory demands
//! `--recursive`.

use crate::commands::{state_path_for, ExecutionMode};
use crate::commands::walk::{remote_join, safe_rel, walk_local, walk_remote};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::ignored::Matcher;
use crate::state::StateFile;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

pub fn run(
    config_path: &Path,
    paths: &[String],
    recursive: bool,
    mode: ExecutionMode,
) -> Result<()> {
    // Primary safety guard: refuse before touching config or the network so a
    // bare `rm` can never wipe anything.
    if paths.is_empty() {
        anyhow::bail!("rm requires at least one path");
    }

    let cfg = Config::load(config_path)?;

    // Validate/normalize every path up front, before opening a connection, so a
    // bad argument fails fast with no side effects.
    let rels: Vec<String> = paths.iter().map(|p| safe_rel(p)).collect::<Result<_>>()?;

    let local_root = cfg.paths.local_root.clone();
    let state_path = state_path_for(&local_root, mode);
    let mut state = StateFile::load_or_default(&state_path)?;
    let matcher = Matcher::new(&cfg.sync.ignore, &local_root)?;

    let mut ftp = Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )?;

    let mut failed = false;
    for rel in &rels {
        let result = if recursive {
            remove_recursive(&mut ftp, &mut state, &cfg, &local_root, &matcher, rel, mode)
        } else {
            remove_file_target(&mut ftp, &mut state, &cfg, &local_root, rel, mode)
        };
        if let Err(e) = result {
            eprintln!("error removing {rel}: {e:#}");
            failed = true;
        }
    }

    // In apply mode, persist state regardless — deletions that succeeded before
    // a later failure still need their records dropped.
    if mode.should_apply() {
        state.save(&state_path)?;
    }

    if failed {
        anyhow::bail!("rm: one or more paths could not be deleted");
    }
    Ok(())
}

/// Delete a single file target from whichever sides have it, plus its state
/// entry. Errors if the path is a directory (needs `--recursive`) or exists on
/// neither side (a typo the user should see).
fn remove_file_target(
    ftp: &mut Ftp,
    state: &mut StateFile,
    cfg: &Config,
    local_root: &Path,
    rel: &str,
    mode: ExecutionMode,
) -> Result<()> {
    let local_full = local_root.join(rel);
    if local_full.is_dir() {
        anyhow::bail!("refusing directory {rel:?}: pass --recursive");
    }

    let remote_path = remote_join(&cfg.paths.remote_root, rel);
    // SIZE is defined for files, so a successful reply means the file exists.
    let on_remote = ftp.size(&remote_path).is_ok();
    let on_local = local_full.is_file();

    if !on_remote && !on_local {
        // SIZE also fails on directories. If we can list the path it's a
        // remote-only directory — point the user at --recursive rather than
        // claiming it doesn't exist.
        if ftp.list(&remote_path).is_ok() {
            anyhow::bail!("refusing directory {rel:?}: pass --recursive");
        }
        anyhow::bail!("no such file on remote or local: {rel}");
    }

    delete_file(
        ftp,
        state,
        &remote_path,
        &local_full,
        rel,
        on_remote,
        on_local,
        mode,
    )?;
    Ok(())
}

/// Recursively delete a subtree: every file under `rel` on either side, then
/// the emptied directories bottom-up. Directory-removal failures (e.g. a
/// directory still holding ignored local files) are warnings, not errors.
fn remove_recursive(
    ftp: &mut Ftp,
    state: &mut StateFile,
    cfg: &Config,
    local_root: &Path,
    matcher: &Matcher,
    rel: &str,
    mode: ExecutionMode,
) -> Result<()> {
    // Enumerate files on both sides, scoped to this subtree. A remote subtree
    // that doesn't exist is not fatal — the local side may still have files.
    let mut remote_files: BTreeSet<String> = BTreeSet::new();
    let _ = walk_remote(ftp, &cfg.paths.remote_root, rel, &mut remote_files);

    let mut local_files: BTreeSet<String> = BTreeSet::new();
    let local_full = local_root.join(rel);
    if local_full.is_dir() {
        walk_local(local_root, &local_full, matcher, &mut local_files)?;
    } else if local_full.is_file() {
        // `rm --recursive <file>` degrades to a plain file delete.
        local_files.insert(rel.to_string());
    }

    let mut all: BTreeSet<String> = BTreeSet::new();
    all.extend(remote_files.iter().cloned());
    all.extend(local_files.iter().cloned());
    if all.is_empty() {
        anyhow::bail!("no such directory on remote or local: {rel}");
    }

    // Directory prefixes discovered while deleting files, so we can rmdir them
    // afterward. BTreeSet order puts a parent before its children (the parent
    // is a shorter prefix), so reverse iteration removes deepest-first.
    let mut dirs: BTreeSet<String> = BTreeSet::new();

    for f in &all {
        let on_remote = remote_files.contains(f);
        let on_local = local_files.contains(f);
        let remote_path = remote_join(&cfg.paths.remote_root, f);
        let local_file = local_root.join(f);
        delete_file(
            ftp,
            state,
            &remote_path,
            &local_file,
            f,
            on_remote,
            on_local,
            mode,
        )?;
        collect_dirs_at_or_below(f, rel, &mut dirs);
    }

    for d in dirs.iter().rev() {
        if mode.is_dry_run() {
            println!("would remove dir {d}/");
            continue;
        }

        let remote_dir = remote_join(&cfg.paths.remote_root, d);
        let remote_ok = match ftp.rmdir(&remote_dir) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("warning: could not remove remote dir {d}/: {e:#}");
                false
            }
        };
        let local_dir = local_root.join(d);
        let local_ok = if local_dir.is_dir() {
            match std::fs::remove_dir(&local_dir) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("warning: could not remove local dir {d}/: {e:#}");
                    false
                }
            }
        } else {
            false
        };
        if remote_ok || local_ok {
            println!("removed dir {d}/");
        }
    }

    Ok(())
}

/// Delete one file from the sides indicated by `on_remote`/`on_local`, drop its
/// state entry, and print the result. The caller guarantees at least one side.
// Paths and presence are independent for each side, so keep them explicit.
#[allow(clippy::too_many_arguments)]
fn delete_file(
    ftp: &mut Ftp,
    state: &mut StateFile,
    remote_path: &str,
    local_full: &Path,
    rel: &str,
    on_remote: bool,
    on_local: bool,
    mode: ExecutionMode,
) -> Result<()> {
    if mode.is_dry_run() {
        println!("would delete ({}) {rel}", sides_label(on_remote, on_local));
        return Ok(());
    }

    if on_remote {
        ftp.rm(remote_path)
            .with_context(|| format!("deleting remote {remote_path}"))?;
    }
    if on_local {
        std::fs::remove_file(local_full)
            .with_context(|| format!("deleting local {}", local_full.display()))?;
    }
    state.files.remove(rel);
    println!("deleted ({}) {rel}", sides_label(on_remote, on_local));
    Ok(())
}

fn sides_label(remote: bool, local: bool) -> &'static str {
    match (remote, local) {
        (true, true) => "remote+local",
        (true, false) => "remote",
        (false, true) => "local",
        (false, false) => unreachable!("caller guarantees at least one side"),
    }
}

/// Collect every directory prefix of file path `f` that lies at or below the
/// deleted root `rel`. Ancestors above `rel` are never included — we only clean
/// up directories we actually emptied. `rel` is trailing-slash-free (guaranteed
/// by `safe_rel`).
fn collect_dirs_at_or_below(f: &str, rel: &str, dirs: &mut BTreeSet<String>) {
    let mut parts: Vec<&str> = f.split('/').collect();
    parts.pop(); // drop the filename
    let mut acc = String::new();
    let below_prefix = format!("{rel}/");
    for part in parts {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        if acc == rel || acc.starts_with(&below_prefix) {
            dirs.insert(acc.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(f: &str, rel: &str) -> Vec<String> {
        let mut set = BTreeSet::new();
        collect_dirs_at_or_below(f, rel, &mut set);
        set.into_iter().collect()
    }

    #[test]
    fn collects_root_and_nested_dirs_but_not_ancestors() {
        // Deleting under "src/old": we clean up src/old and src/old/sub, never
        // the ancestor "src".
        assert_eq!(dirs("src/old/sub/a.html", "src/old"), vec!["src/old", "src/old/sub"]);
    }

    #[test]
    fn file_directly_in_root_collects_only_root() {
        assert_eq!(dirs("src/old/a.html", "src/old"), vec!["src/old"]);
    }

    #[test]
    fn single_file_target_collects_no_dirs() {
        assert!(dirs("notes.txt", "notes.txt").is_empty());
    }
}
