//! `zed-ftp pull` — one-way download from remote into the local mirror.
//!
//! Pull is asymmetric with push by design: it only ever writes local files;
//! it never deletes locally just because the remote is missing a file. That
//! way an accidentally-empty remote (or an interrupted upload by someone
//! else) cannot wipe your working tree.

use crate::commands::walk::{remote_join, walk_local, walk_remote};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::{hash_bytes, hash_file};
use crate::ignored::Matcher;
use crate::state::{classify, FileRecord, FileState, StateFile};
use anyhow::{Context, Result};
use chrono::Utc;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn run(config_path: &Path, paths: &[String], force: bool) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let local_root = cfg.paths.local_root.clone();
    let state_path = local_root.join(".zed-ftp").join("state.json");
    let mut state = StateFile::load_or_default(&state_path)?;

    let matcher = Matcher::new(&cfg.sync.ignore, &local_root)?;

    let mut ftp = Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )?;

    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    walk_local(&local_root, &local_root, &matcher, &mut local_paths)?;
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    walk_remote(&mut ftp, &cfg.paths.remote_root, "", &mut remote_paths)?;

    // Determine which relative paths to consider. With explicit args, we
    // restrict to those (still applying classify+decide). Without args we
    // walk every path that `status` would consider.
    let targets: Vec<String> = if paths.is_empty() {
        let mut all: BTreeSet<String> = BTreeSet::new();
        all.extend(local_paths.iter().cloned());
        all.extend(remote_paths.iter().cloned());
        for k in state.files.keys() {
            all.insert(k.clone());
        }
        all.into_iter().collect()
    } else {
        let mut out = Vec::new();
        for p in paths {
            let rel = normalize_rel(p);
            if rel.is_empty() || Path::new(&rel).is_absolute() || rel.split('/').any(|c| c == "..") {
                anyhow::bail!("refusing path {p:?}: must be a relative path under local_root with no '..' segments");
            }
            if matcher.is_ignored(&local_root.join(&rel), false) {
                eprintln!("skip (ignored by .zed-ftp.toml): {rel}");
                continue;
            }
            out.push(rel);
        }
        out
    };

    let mut had_conflict = false;

    for rel in &targets {
        let on_local = local_paths.contains(rel);
        let on_remote = remote_paths.contains(rel);

        if !on_local && !on_remote {
            // Stale state entry or path that exists on neither side. Nothing
            // to pull.
            eprintln!("skip (not on local or remote): {rel}");
            continue;
        }

        let local_hash = if on_local {
            Some(hash_file(&local_root.join(rel))?)
        } else {
            None
        };
        let remote_path = remote_join(&cfg.paths.remote_root, rel);
        // Download once per file we might act on. We still need the bytes
        // for the actual write, so keeping them avoids a second round-trip.
        let remote_bytes = if on_remote {
            Some(
                ftp.download(&remote_path)
                    .with_context(|| format!("downloading {remote_path}"))?,
            )
        } else {
            None
        };
        let remote_hash = remote_bytes.as_deref().map(hash_bytes);

        let known = state.files.get(rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash.as_deref(), known);

        match st {
            FileState::InSync => {
                // Nothing to write. Local matches remote.
            }
            FileState::LocalOnly => {
                // Pull is one-way: we don't delete local files because the
                // remote is missing them. Skip.
            }
            FileState::RemoteOnly | FileState::RemoteChanged => {
                let bytes = remote_bytes
                    .as_deref()
                    .expect("remote_bytes set when on_remote is true");
                let new_hash = remote_hash.as_deref().expect("remote_hash matches remote_bytes");
                download_one(&mut ftp, &mut state, &local_root.join(rel), rel, &remote_path, bytes, new_hash)?;
                println!("pulled {rel}");
            }
            FileState::LocalChanged | FileState::BothChanged | FileState::Untracked => {
                // Untracked = both sides have a file but no record of a prior sync.
                // Design action matrix treats this as "as if both-changed": refuse
                // without --force so the user makes an explicit choice.
                if force {
                    let bytes = remote_bytes
                        .as_deref()
                        .expect("remote_bytes set when on_remote is true");
                    let new_hash = remote_hash.as_deref().expect("remote_hash matches remote_bytes");
                    eprintln!("overwriting local with remote (--force): {rel}");
                    download_one(&mut ftp, &mut state, &local_root.join(rel), rel, &remote_path, bytes, new_hash)?;
                } else {
                    eprintln!(
                        "conflict ({:?}, would overwrite local edits): {rel} — pass --force to override",
                        st
                    );
                    had_conflict = true;
                }
            }
        }
    }

    // Save state even if we hit a conflict — partial progress is still
    // worth persisting (e.g. clean RemoteChanged pulls that succeeded
    // before the conflict file).
    state.save(&state_path)?;

    if had_conflict {
        anyhow::bail!("pull aborted: one or more files have local changes (use --force to overwrite)");
    }

    Ok(())
}

/// Normalize a user-supplied path argument into the relative form used as
/// state-file keys: forward slashes, no leading `./`.
fn normalize_rel(p: &str) -> String {
    let s = p.replace('\\', "/");
    s.trim_start_matches("./").to_string()
}

/// Write `bytes` to `local_path` atomically (via temp + rename) and refresh
/// the corresponding state entry. Shared with the sync command — both pull
/// and sync need exactly this "write + record the new hash" sequence on the
/// remote-wins branch.
pub fn download_one(
    ftp: &mut Ftp,
    state: &mut StateFile,
    local_path: &Path,
    rel: &str,
    remote_path: &str,
    bytes: &[u8],
    new_hash: &str,
) -> Result<()> {
    write_local_atomic(local_path, bytes)?;
    update_state_after_pull(state, rel, ftp, remote_path, new_hash, bytes.len() as u64)?;
    Ok(())
}

/// Write `bytes` to `path` via a sibling `.tmp.zedftp` file and `rename` so
/// readers never observe a half-written file. The temp file is removed on
/// any failure before the rename.
fn write_local_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let tmp = tmp_path(path);
    if let Err(e) = std::fs::write(&tmp, bytes)
        .with_context(|| format!("writing temp file {}", tmp.display()))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp.zedftp");
    PathBuf::from(s)
}

/// Refresh the state entry for `rel` after a successful download+write.
/// `new_hash` is the hash of the remote bytes we just wrote locally;
/// `size` is the byte count we already have in hand (no extra round-trip).
fn update_state_after_pull(
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
