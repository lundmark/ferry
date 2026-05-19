//! `zed-ftp sync` — bidirectional reconciliation in a single pass.
//!
//! Sync walks the union of local + remote + state and applies the design's
//! action matrix per file:
//!
//! | State          | Action                                                    |
//! |----------------|-----------------------------------------------------------|
//! | InSync         | noop                                                      |
//! | LocalChanged   | upload                                                    |
//! | RemoteChanged  | download                                                  |
//! | LocalOnly      | upload (new remote file)                                  |
//! | RemoteOnly     | download (new local file)                                 |
//! | BothChanged    | refuse without --force; with --force, local wins (upload) |
//! | Untracked      | refuse without --force; with --force, local wins (upload) |
//!
//! "Local wins on --force" matches the documented sync semantics: when the
//! user says "just sync it already," last-write-wins from the local side
//! since that's the side the user is actively editing.

use crate::commands::pull::download_one;
use crate::commands::push::upload_one;
use crate::commands::walk::{remote_join, walk_local, walk_remote};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::{hash_bytes, hash_file};
use crate::ignored::Matcher;
use crate::state::{classify, FileState, StateFile};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

pub fn run(config_path: &Path, force: bool) -> Result<()> {
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

    // Union of every relative path we know about from any source.
    let mut targets: BTreeSet<String> = BTreeSet::new();
    targets.extend(local_paths.iter().cloned());
    targets.extend(remote_paths.iter().cloned());
    for k in state.files.keys() {
        targets.insert(k.clone());
    }

    let mut had_conflict = false;

    for rel in &targets {
        let on_local = local_paths.contains(rel);
        let on_remote = remote_paths.contains(rel);

        if !on_local && !on_remote {
            // Stale state entry — exists in neither place. Nothing to do.
            eprintln!("skip (not on local or remote): {rel}");
            continue;
        }

        // Compute local hash from disk (cheap; we may still need bytes for upload).
        let local_hash = if on_local {
            Some(hash_file(&local_root.join(rel))?)
        } else {
            None
        };
        let remote_path = remote_join(&cfg.paths.remote_root, rel);
        // Pull remote bytes once. We need them both to hash and to write
        // locally on the download branch; uploading uses the local copy.
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
                // Nothing to do; both sides agree.
            }
            FileState::LocalChanged | FileState::LocalOnly => {
                let bytes = std::fs::read(local_root.join(rel))
                    .with_context(|| format!("reading local {}", local_root.join(rel).display()))?;
                let new_hash = local_hash
                    .as_deref()
                    .expect("local_hash set when on_local is true");
                upload_one(&mut ftp, &mut state, rel, &remote_path, &bytes, new_hash)?;
                println!("uploaded {rel}");
            }
            FileState::RemoteChanged | FileState::RemoteOnly => {
                let bytes = remote_bytes
                    .as_deref()
                    .expect("remote_bytes set when on_remote is true");
                let new_hash = remote_hash
                    .as_deref()
                    .expect("remote_hash matches remote_bytes");
                download_one(
                    &mut ftp,
                    &mut state,
                    &local_root.join(rel),
                    rel,
                    &remote_path,
                    bytes,
                    new_hash,
                )?;
                println!("downloaded {rel}");
            }
            FileState::BothChanged | FileState::Untracked => {
                // Conflict: both sides moved away from the last known state
                // (or there is no known state and both sides have something).
                // Refuse unless --force, in which case the design says local
                // wins — sync's "force" is the user telling us to just push
                // their working copy as the canonical version.
                if force {
                    let bytes = std::fs::read(local_root.join(rel))
                        .with_context(|| format!("reading local {}", local_root.join(rel).display()))?;
                    let new_hash = local_hash
                        .as_deref()
                        .expect("local_hash set when on_local is true");
                    eprintln!("overwriting remote with local (--force): {rel}");
                    upload_one(&mut ftp, &mut state, rel, &remote_path, &bytes, new_hash)?;
                } else {
                    eprintln!(
                        "conflict ({:?}, local and remote diverged): {rel} — pass --force to take local",
                        st
                    );
                    had_conflict = true;
                }
            }
        }
    }

    // Persist progress even on conflict so the clean files don't have to be
    // re-hashed next run. Matches push/pull behavior.
    state.save(&state_path)?;

    if had_conflict {
        anyhow::bail!("sync aborted: one or more files diverged on both sides (use --force to take local)");
    }

    Ok(())
}
