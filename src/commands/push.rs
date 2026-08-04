//! `ferry push` — one-way upload from the local mirror to remote.
//!
//! Push is asymmetric with pull by design: it only ever writes remote files;
//! it never deletes a remote file because the local mirror is missing it. A
//! locally-missing file is treated as "not yours to delete" — `rm` is its
//! own deliberate command.

use crate::commands::remote_hash;
use crate::commands::walk::{collect_remote_arg, remote_join, safe_rel, walk_local, walk_remote};
use crate::commands::{state_path_for, ExecutionMode};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::hash_bytes;
use crate::ignored::Matcher;
use crate::state::{classify, FileRecord, FileState, StateFile};
use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::BTreeSet;
use std::path::Path;

pub fn run(config_path: &Path, paths: &[String], force: bool, mode: ExecutionMode) -> Result<()> {
    let cfg = Config::load(config_path)?;
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

    // Scope the walks to the paths we actually care about. Bare `push`
    // walks the whole tree; `push <folder>` walks only that subtree,
    // `push <file>` skips the walks entirely.
    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    if paths.is_empty() {
        walk_local(&local_root, &local_root, &matcher, &mut local_paths)?;
        walk_remote(&mut ftp, &cfg.paths.remote_root, "", &mut remote_paths)?;
    } else {
        for p in paths {
            let rel_no_slash = safe_rel(p)?;
            let local_full = local_root.join(&rel_no_slash);
            if local_full.is_dir() {
                walk_local(&local_root, &local_full, &matcher, &mut local_paths)?;
            } else if local_full.is_file() {
                local_paths.insert(rel_no_slash.clone());
            }
            collect_remote_arg(
                &mut ftp,
                &cfg.paths.remote_root,
                &rel_no_slash,
                &mut remote_paths,
            );
        }
    }

    // Build the target set. With explicit args, restrict targets to the
    // union of files under those args (via scoped walks above) — this
    // gives folder-expansion semantics matching `pull`.
    let targets: Vec<String> = {
        let mut all: BTreeSet<String> = BTreeSet::new();
        all.extend(local_paths.iter().cloned());
        all.extend(remote_paths.iter().cloned());
        if paths.is_empty() {
            for k in state.files.keys() {
                all.insert(k.clone());
            }
        }
        all.into_iter()
            .filter(|rel| !matcher.is_ignored(&local_root.join(rel), false))
            .collect()
    };

    let mut had_conflict = false;

    for rel in &targets {
        let on_local = local_paths.contains(rel);
        let on_remote = remote_paths.contains(rel);

        if !on_local && !on_remote {
            // Stale state entry or path that exists on neither side. Nothing
            // to push.
            eprintln!("skip (not on local or remote): {rel}");
            continue;
        }

        // Read local bytes once (we need them for both hashing and upload),
        // but only when there's actually a local file to consider.
        let local_bytes = if on_local {
            Some(
                std::fs::read(local_root.join(rel))
                    .with_context(|| format!("reading local {}", local_root.join(rel).display()))?,
            )
        } else {
            None
        };
        let local_hash = local_bytes.as_deref().map(hash_bytes);

        let remote_path = remote_join(&cfg.paths.remote_root, rel);
        // Hash the remote, using the MDTM/SIZE fast path when possible.
        // Push only needs the hash for classification — never the bytes —
        // so request `want_bytes=false`.
        let remote_hash = if on_remote {
            Some(remote_hash::compute(&mut ftp, &mut state, rel, &remote_path, false)?.sha256)
        } else {
            None
        };

        let known = state.files.get(rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash.as_deref(), known);

        match st {
            FileState::InSync => {
                // Nothing to upload. Local matches remote.
            }
            FileState::RemoteOnly => {
                // Push is one-way: don't delete the remote file just because
                // the local mirror is missing it. The user can `rm` deliberately.
            }
            FileState::LocalOnly | FileState::LocalChanged => {
                let bytes = local_bytes
                    .as_deref()
                    .expect("local_bytes set when on_local is true");
                let new_hash = local_hash.as_deref().expect("local_hash matches local_bytes");
                upload_one(&mut ftp, &mut state, rel, &remote_path, bytes, new_hash, mode)?;
                println!("{} {rel}", if mode.is_dry_run() { "would push" } else { "pushed" });
            }
            FileState::RemoteChanged | FileState::BothChanged | FileState::Untracked => {
                // Untracked = both sides have a file but no record of a prior sync.
                // Design action matrix treats this as "as if both-changed": refuse
                // without --force so the user makes an explicit choice.
                if force {
                    let bytes = local_bytes
                        .as_deref()
                        .expect("local_bytes set when on_local is true");
                    let new_hash = local_hash.as_deref().expect("local_hash matches local_bytes");
                    if mode.is_dry_run() {
                        eprintln!("would overwrite remote with local (--force): {rel}");
                    } else {
                        eprintln!("overwriting remote with local (--force): {rel}");
                    }
                    upload_one(&mut ftp, &mut state, rel, &remote_path, bytes, new_hash, mode)?;
                } else {
                    eprintln!(
                        "conflict ({:?}, would overwrite remote edits): {rel} — pass --force to override",
                        st
                    );
                    had_conflict = true;
                }
            }
        }
    }

    // Save state even if we hit a conflict — partial progress is still
    // worth persisting (e.g. clean LocalChanged pushes that succeeded
    // before the conflict file).
    if mode.should_apply() {
        state.save(&state_path)?;
    }

    if had_conflict {
        // Tag as `Exit::Conflict` so `main()` returns exit code 2 — Zed's
        // tasks.json uses that to surface a "needs --force" message rather
        // than a generic failure.
        return Err(crate::error::Exit::Conflict(
            "push aborted: one or more files have remote changes (use --force to overwrite)".into(),
        )
        .into());
    }

    Ok(())
}

/// Upload `bytes` to `remote_path` atomically (via temp + rename) and refresh
/// the corresponding state entry. Shared with the sync command — both push
/// and sync need exactly this "upload + record the new hash" sequence on the
/// local-wins branch.
pub fn upload_one(
    ftp: &mut Ftp,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    bytes: &[u8],
    new_hash: &str,
    mode: ExecutionMode,
) -> Result<()> {
    if mode.is_dry_run() {
        return Ok(());
    }
    upload_remote_atomic(ftp, remote_path, bytes)?;
    update_state_after_push(state, rel, ftp, remote_path, new_hash, bytes.len() as u64)
}

/// Upload `bytes` to `remote_path` via a sibling `.tmp.zedftp` file and FTP
/// rename so readers never observe a half-written file. The temp file is
/// removed on any failure before the rename.
fn upload_remote_atomic(ftp: &mut Ftp, remote_path: &str, bytes: &[u8]) -> Result<()> {
    ensure_remote_parents(ftp, remote_path)?;
    let tmp = format!("{remote_path}.tmp.zedftp");
    if let Err(e) = ftp.upload_bytes(&tmp, bytes)
        .with_context(|| format!("uploading temp {tmp}"))
    {
        let _ = ftp.rm(&tmp);
        return Err(e);
    }
    if let Err(e) = ftp.rename(&tmp, remote_path)
        .with_context(|| format!("renaming {tmp} -> {remote_path}"))
    {
        let _ = ftp.rm(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Walk the prefix segments of `remote_path` and `mkdir` each one. Tolerates
/// already-existing directories via `Ftp::mkdir`'s built-in idempotency.
fn ensure_remote_parents(ftp: &mut Ftp, remote_path: &str) -> Result<()> {
    // Find the last '/'; everything before it is the directory portion.
    let Some(dir_end) = remote_path.rfind('/') else {
        // No slash, no parent to create.
        return Ok(());
    };
    let dir = &remote_path[..dir_end];
    if dir.is_empty() {
        // File at root, e.g. "/foo.txt" → dir = "" → nothing to create.
        return Ok(());
    }
    // Walk prefixes: e.g. "/a/b/c" → "/a", "/a/b", "/a/b/c".
    // We skip the leading empty segment so absolute paths work.
    let mut segments = dir.split('/').peekable();
    let leading_slash = dir.starts_with('/');
    let mut acc = String::new();
    // Consume the empty leading segment for absolute paths.
    if leading_slash {
        let _ = segments.next();
    }
    for seg in segments {
        if seg.is_empty() {
            // Defensive: skip empty segments from double slashes.
            continue;
        }
        if leading_slash || !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        ftp.mkdir(&acc)?;
    }
    Ok(())
}

/// Refresh the state entry for `rel` after a successful upload+rename.
/// `new_hash` is the hash of the local bytes we just pushed; `size` is the
/// byte count we already have in hand (no extra round-trip).
fn update_state_after_push(
    state: &mut StateFile,
    rel: &str,
    ftp: &mut Ftp,
    remote_path: &str,
    new_hash: &str,
    size: u64,
) -> Result<()> {
    let remote_mtime = ftp.mtime(remote_path)
        .with_context(|| format!("fetching mtime for {remote_path}"))?;
    state.files.insert(
        rel.to_string(),
        FileRecord {
            sha256: new_hash.to_string(),
            size,
            remote_mtime,
            last_synced: Utc::now(),
        },
    );
    Ok(())
}
