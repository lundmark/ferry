//! `ferry pull` — one-way download from remote into the local mirror.
//!
//! Pull is asymmetric with push by design: it only ever writes local files;
//! it never deletes locally just because the remote is missing a file. That
//! way an accidentally-empty remote (or an interrupted upload by someone
//! else) cannot wipe your working tree.

mod prepared;

pub use prepared::{
    LocalIdentity, PreparedPull, RemoteFile, apply_prepared_pull, apply_prepared_pull_if,
    fetch_remote_one, prepare_force_pull_one, prepare_pull_one, pull_one,
};

use crate::commands::remote_hash;
use crate::commands::walk::{collect_remote_arg, remote_join, safe_arg, walk_local, walk_remote};
use crate::commands::{ExecutionMode, state_path_for};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::hash_file;
use crate::ignored::Matcher;
use crate::state::{FileRecord, FileState, StateFile, classify};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn run(config_path: &Path, paths: &[String], force: bool, mode: ExecutionMode) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let local_root = cfg.paths.local_root.clone();
    let paths: Vec<String> = paths
        .iter()
        .map(|path| safe_arg(&local_root, path))
        .collect::<Result<_>>()?;
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

    // Scope the walks to the paths we actually care about, and build the
    // target set in one pass so we emit exactly one status message per arg.
    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    let targets: Vec<String> = if paths.is_empty() {
        walk_local(&local_root, &local_root, &matcher, &mut local_paths)?;
        walk_remote(&mut ftp, &cfg.paths.remote_root, "", &mut remote_paths)?;
        let mut all: BTreeSet<String> = BTreeSet::new();
        all.extend(local_paths.iter().cloned());
        all.extend(remote_paths.iter().cloned());
        for k in state.files.keys() {
            all.insert(k.clone());
        }
        all.into_iter().collect()
    } else {
        let mut out: BTreeSet<String> = BTreeSet::new();
        for rel in &paths {
            let rel_no_slash = rel.trim_end_matches('/');
            let local_full = local_root.join(rel_no_slash);
            let mut found_here = 0usize;

            // Local: directory, file, or missing.
            if local_full.is_dir() {
                let before = local_paths.len();
                walk_local(&local_root, &local_full, &matcher, &mut local_paths)?;
                found_here += local_paths.len() - before;
            } else if local_full.is_file()
                && !matcher.is_ignored(&local_full, false)
                && local_paths.insert(rel_no_slash.to_string())
            {
                found_here += 1;
            }

            // Remote: subtree walk, or single-file resolution.
            found_here += collect_remote_arg(
                &mut ftp,
                &cfg.paths.remote_root,
                rel_no_slash,
                &mut remote_paths,
            );

            if found_here == 0 {
                eprintln!("skip (not on local or remote): {rel_no_slash}");
                continue;
            }

            // Add matches under this arg to targets. Exact match first,
            // then prefix expansion for the folder case.
            if (local_paths.contains(rel_no_slash) || remote_paths.contains(rel_no_slash))
                && !matcher.is_ignored(&local_root.join(rel_no_slash), false)
            {
                out.insert(rel_no_slash.to_string());
            }
            let prefix = format!("{rel_no_slash}/");
            for path in local_paths.iter().chain(remote_paths.iter()) {
                if path.starts_with(&prefix) && !matcher.is_ignored(&local_root.join(path), false) {
                    out.insert(path.clone());
                }
            }
        }
        out.into_iter().collect()
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
        // Use the MDTM/SIZE fast path: skip downloading entirely when the
        // server's (mtime, size) match the cached state — that case is
        // exactly the InSync branch below, which doesn't need the bytes.
        // When the fast path can't fire we ask for bytes (`want_bytes=true`)
        // so we have them in hand for the actual local write.
        let rh = if on_remote {
            Some(remote_hash::compute(
                &mut ftp,
                &mut state,
                rel,
                &remote_path,
                true,
            )?)
        } else {
            None
        };
        let remote_hash_str = rh.as_ref().map(|r| r.sha256.clone());

        let known = state.files.get(rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash_str.as_deref(), known);

        match st {
            FileState::InSync => {
                // Nothing to write. Local matches remote.
            }
            FileState::LocalOnly => {
                // Pull is one-way: we don't delete local files because the
                // remote is missing them. Skip.
            }
            FileState::RemoteOnly | FileState::RemoteChanged => {
                // We need the actual remote bytes to write locally. If the
                // fast path fired (from_cache=true), we got here with the
                // hash but no bytes — which can only happen if state has a
                // record AND it matches AND yet classify said the remote
                // changed. That implies the local file diverged from `known`
                // while remote matches `known` (LocalChanged) — not this
                // branch. So in practice rh.bytes is Some here. Defensive
                // fallback: if bytes are missing, fetch them now.
                let rh_inner = rh.as_ref().expect("rh set when on_remote is true");
                let bytes_owned: Vec<u8> = match &rh_inner.bytes {
                    Some(b) => b.clone(),
                    None => ftp
                        .download(&remote_path)
                        .with_context(|| format!("downloading {remote_path}"))?,
                };
                download_one(
                    &mut ftp,
                    &mut state,
                    &local_root.join(rel),
                    rel,
                    &remote_path,
                    &bytes_owned,
                    &rh_inner.sha256,
                    mode,
                )?;
                println!(
                    "{} {rel}",
                    if mode.is_dry_run() {
                        "would pull"
                    } else {
                        "pulled"
                    }
                );
            }
            FileState::LocalChanged | FileState::BothChanged | FileState::Untracked => {
                // Untracked = both sides have a file but no record of a prior sync.
                // Design action matrix treats this as "as if both-changed": refuse
                // without --force so the user makes an explicit choice.
                if force {
                    let rh_inner = rh.as_ref().expect("rh set when on_remote is true");
                    let bytes_owned: Vec<u8> = match &rh_inner.bytes {
                        Some(b) => b.clone(),
                        None => ftp
                            .download(&remote_path)
                            .with_context(|| format!("downloading {remote_path}"))?,
                    };
                    if mode.is_dry_run() {
                        eprintln!("would overwrite local with remote (--force): {rel}");
                    } else {
                        eprintln!("overwriting local with remote (--force): {rel}");
                    }
                    download_one(
                        &mut ftp,
                        &mut state,
                        &local_root.join(rel),
                        rel,
                        &remote_path,
                        &bytes_owned,
                        &rh_inner.sha256,
                        mode,
                    )?;
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
    if mode.should_apply() {
        state.save(&state_path)?;
    }

    if had_conflict {
        // Tag as `Exit::Conflict` so `main()` returns exit code 2 — Zed's
        // tasks.json uses that to surface a "needs --force" message rather
        // than a generic failure.
        return Err(crate::error::Exit::Conflict(
            "pull aborted: one or more files have local changes (use --force to overwrite)".into(),
        )
        .into());
    }

    Ok(())
}

/// In [`ExecutionMode::Apply`], write `bytes` to `local_path` atomically (via
/// temp + rename) and refresh the corresponding state entry. In
/// [`ExecutionMode::DryRun`], perform neither mutation.
///
/// Shared with the sync command on its remote-wins branch.
// The state update needs both local and remote identities plus the downloaded
// payload metadata; wrapping these one-to-one inputs would only hide them.
#[allow(clippy::too_many_arguments)]
pub fn download_one(
    ftp: &mut Ftp,
    state: &mut StateFile,
    local_path: &Path,
    rel: &str,
    remote_path: &str,
    bytes: &[u8],
    new_hash: &str,
    mode: ExecutionMode,
) -> Result<()> {
    if mode.is_dry_run() {
        return Ok(());
    }
    write_local_atomic(local_path, bytes)?;
    let remote_mtime = ftp
        .mtime(remote_path)
        .with_context(|| format!("fetching mtime for {remote_path}"))?;
    record_download(state, rel, new_hash, bytes.len() as u64, remote_mtime);
    Ok(())
}

/// Write `bytes` to `path` via a sibling `.tmp.zedftp` file and `rename` so
/// readers never observe a half-written file. The temp file is removed on
/// any failure before the rename.
fn write_local_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    stage_local_write(path, bytes)?.commit()
}

struct StagedLocalWrite {
    tmp: PathBuf,
    target: PathBuf,
    committed: bool,
}

fn stage_local_write(path: &Path, bytes: &[u8]) -> Result<StagedLocalWrite> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }

    let tmp = tmp_path(path);
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("flushing temp file {}", tmp.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }

    Ok(StagedLocalWrite {
        tmp,
        target: path.to_path_buf(),
        committed: false,
    })
}

impl StagedLocalWrite {
    fn commit(mut self) -> Result<()> {
        std::fs::rename(&self.tmp, &self.target).with_context(|| {
            format!(
                "renaming {} -> {}",
                self.tmp.display(),
                self.target.display()
            )
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedLocalWrite {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp.zedftp");
    PathBuf::from(s)
}

fn record_download(
    state: &mut StateFile,
    rel: &str,
    new_hash: &str,
    size: u64,
    remote_mtime: DateTime<Utc>,
) {
    state.files.insert(
        rel.to_string(),
        FileRecord {
            sha256: new_hash.to_string(),
            size,
            remote_mtime,
            last_synced: Utc::now(),
        },
    );
}
