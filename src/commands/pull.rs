//! `ferry pull` — one-way download from remote into the local mirror.
//!
//! Pull is asymmetric with push by design: it only ever writes local files;
//! it never deletes locally just because the remote is missing a file. That
//! way an accidentally-empty remote (or an interrupted upload by someone
//! else) cannot wipe your working tree.

// Task 4 exposes guarded primitives that the scoped engine wires in Task 5.
#![allow(dead_code)]

mod prepared;

pub use prepared::{
    LocalIdentity, PreparedPull, RemoteFile, apply_prepared_pull, apply_prepared_pull_if,
    fetch_remote_one, prepare_force_pull_one, prepare_pull_one, pull_one,
};

use crate::commands::file_transfer::LocalPathExpectation;
use crate::commands::remote_hash;
use crate::commands::remote_hash::RemoteHash;
use crate::commands::sync::commit::{CommitDecision, CommitGate, UnconditionalCommitGate};
use crate::commands::walk::{collect_remote_arg, remote_join, safe_arg, walk_local, walk_remote};
use crate::commands::{ExecutionMode, state_path_for};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::{hash_bytes, hash_file};
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
    let remote_mtime = ftp
        .mtime(remote_path)
        .with_context(|| format!("fetching mtime for {remote_path}"))?;
    let mut staged = Some(stage_local_write(local_path, bytes)?);
    let mut mutation = || {
        staged
            .take()
            .ok_or_else(|| anyhow::anyhow!("download commit mutation invoked more than once"))?
            .commit()?;
        record_download(state, rel, new_hash, bytes.len() as u64, remote_mtime);
        Ok(())
    };
    UnconditionalCommitGate.commit(&mut mutation)?;
    Ok(())
}

#[derive(Debug)]
pub(crate) struct ExpectedLocalDestination {
    snapshot: LocalPathExpectation,
}

impl ExpectedLocalDestination {
    pub(crate) fn capture(local_root: &Path, local_path: &Path) -> Result<Self> {
        Ok(Self {
            snapshot: LocalPathExpectation::capture(local_root, local_path)?,
        })
    }

    fn verify_unchanged(&self, local_path: &Path) -> Result<()> {
        self.snapshot
            .verify_supplied_path(local_path)
            .with_context(|| format!("local destination changed for {}", local_path.display()))?;
        self.snapshot
            .verify_unchanged()
            .with_context(|| format!("local destination changed for {}", local_path.display()))?;
        if !self.snapshot.is_file_or_missing() {
            anyhow::bail!(
                "local destination changed for {}: not a file",
                local_path.display()
            );
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn download_one_guarded(
    state: &mut StateFile,
    local_path: &Path,
    rel: &str,
    remote: &RemoteHash,
    expected: &ExpectedLocalDestination,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<CommitDecision> {
    if mode.is_dry_run() {
        return Ok(CommitDecision::Committed);
    }
    let bytes = remote
        .bytes
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("remote payload missing for guarded download {rel}"))?;
    if bytes.len() as u64 != remote.size || hash_bytes(bytes) != remote.sha256 {
        anyhow::bail!("remote payload changed before guarded download {rel}");
    }

    expected.verify_unchanged(local_path)?;
    let mut staged = Some(stage_local_write_scoped(expected, bytes)?);
    let mut mutation = || {
        expected.verify_unchanged(local_path)?;
        staged
            .take()
            .ok_or_else(|| anyhow::anyhow!("download commit mutation invoked more than once"))?
            .commit()?;
        record_download(state, rel, &remote.sha256, remote.size, remote.mtime);
        Ok(())
    };
    gate.commit(&mut mutation)
}

/// Write `bytes` to `path` via a sibling `.tmp.zedftp` file and `rename` so
/// readers never observe a half-written file. The temp file is removed on
/// any failure before the rename.
pub(crate) struct StagedLocalWrite {
    tmp: PathBuf,
    target: PathBuf,
    created_dirs: Vec<PathBuf>,
    committed: bool,
}

pub(crate) fn stage_local_write(path: &Path, bytes: &[u8]) -> Result<StagedLocalWrite> {
    let created_dirs = create_missing_parent_dirs(path)?;
    let tmp = tmp_path(path);
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        Ok(file) => file,
        Err(error) => {
            remove_created_dirs(&created_dirs);
            return Err(error).with_context(|| format!("creating temp file {}", tmp.display()));
        }
    };
    let staged = StagedLocalWrite {
        tmp,
        target: path.to_path_buf(),
        created_dirs,
        committed: false,
    };

    let write_result = (|| -> Result<()> {
        file.write_all(bytes)
            .with_context(|| format!("writing temp file {}", staged.tmp.display()))?;
        file.flush()
            .with_context(|| format!("flushing temp file {}", staged.tmp.display()))?;
        Ok(())
    })();
    drop(file);
    write_result?;

    Ok(staged)
}

fn stage_local_write_scoped(
    expected: &ExpectedLocalDestination,
    bytes: &[u8],
) -> Result<StagedLocalWrite> {
    expected.snapshot.verify_anchor()?;
    let target = expected.snapshot.resolved_path();
    let tmp = tmp_path(&target);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating temp file {}", tmp.display()))?;
    let staged = StagedLocalWrite {
        tmp,
        target,
        created_dirs: Vec::new(),
        committed: false,
    };

    if let Err(error) = expected.snapshot.verify_anchor() {
        drop(file);
        drop(staged);
        return Err(error).context("local target parent changed after staging");
    }

    let write_result = (|| -> Result<()> {
        file.write_all(bytes)
            .with_context(|| format!("writing temp file {}", staged.tmp.display()))?;
        file.flush()
            .with_context(|| format!("flushing temp file {}", staged.tmp.display()))?;
        Ok(())
    })();
    drop(file);
    write_result?;
    Ok(staged)
}

fn create_missing_parent_dirs(path: &Path) -> Result<Vec<PathBuf>> {
    let Some(parent) = path.parent() else {
        return Ok(Vec::new());
    };
    let mut missing = Vec::new();
    let mut current = parent;
    while !current.as_os_str().is_empty() && !current.exists() {
        missing.push(current.to_path_buf());
        let Some(next) = current.parent() else {
            break;
        };
        current = next;
    }

    let mut created = Vec::new();
    for directory in missing.iter().rev() {
        match std::fs::create_dir(directory) {
            Ok(()) => created.push(directory.clone()),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(error) => {
                remove_created_dirs(&created);
                return Err(error)
                    .with_context(|| format!("creating parent dir {}", directory.display()));
            }
        }
    }
    Ok(created)
}

fn remove_created_dirs(created_dirs: &[PathBuf]) {
    for directory in created_dirs.iter().rev() {
        let _ = std::fs::remove_dir(directory);
    }
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
            remove_created_dirs(&self.created_dirs);
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

#[cfg(test)]
mod staging_tests {
    use super::{ExpectedLocalDestination, download_one_guarded, stage_local_write, tmp_path};
    use crate::commands::ExecutionMode;
    use crate::commands::remote_hash::RemoteHash;
    use crate::commands::sync::commit::{CommitDecision, CommitGate, UnconditionalCommitGate};
    use crate::hash::hash_bytes;
    use crate::state::{FileRecord, StateFile};
    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn remote(bytes: &[u8]) -> RemoteHash {
        RemoteHash {
            sha256: hash_bytes(bytes),
            size: bytes.len() as u64,
            mtime: Utc.with_ymd_and_hms(2026, 8, 10, 9, 0, 0).unwrap(),
            from_cache: false,
            bytes: Some(bytes.to_vec()),
            pre_download: None,
        }
    }

    fn old_record() -> FileRecord {
        FileRecord {
            sha256: hash_bytes(b"old state"),
            size: b"old state".len() as u64,
            remote_mtime: Utc.with_ymd_and_hms(2026, 8, 9, 9, 0, 0).unwrap(),
            last_synced: Utc.with_ymd_and_hms(2026, 8, 9, 9, 1, 0).unwrap(),
        }
    }

    struct CancelAfterStaging {
        tmp: PathBuf,
        saw_temp: AtomicBool,
    }

    impl CommitGate for CancelAfterStaging {
        fn is_current(&self) -> bool {
            true
        }

        fn commit(&self, _mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            self.saw_temp.store(self.tmp.is_file(), Ordering::SeqCst);
            Ok(CommitDecision::Cancelled)
        }
    }

    #[derive(Clone, Copy)]
    enum HookOrder {
        Before,
        After,
    }

    struct HookGate {
        order: HookOrder,
        hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl HookGate {
        fn before(hook: impl FnOnce() + Send + 'static) -> Self {
            Self {
                order: HookOrder::Before,
                hook: Mutex::new(Some(Box::new(hook))),
            }
        }

        fn after(hook: impl FnOnce() + Send + 'static) -> Self {
            Self {
                order: HookOrder::After,
                hook: Mutex::new(Some(Box::new(hook))),
            }
        }

        fn run_hook(&self) {
            self.hook.lock().unwrap().take().unwrap()();
        }
    }

    impl CommitGate for HookGate {
        fn is_current(&self) -> bool {
            true
        }

        fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            if matches!(self.order, HookOrder::Before) {
                self.run_hook();
            }
            mutation()?;
            if matches!(self.order, HookOrder::After) {
                self.run_hook();
            }
            Ok(CommitDecision::Committed)
        }
    }

    struct PanicGate;

    impl CommitGate for PanicGate {
        fn is_current(&self) -> bool {
            panic!("dry run must not inspect the gate")
        }

        fn commit(&self, _mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
            panic!("dry run must not claim the gate")
        }
    }

    #[test]
    fn guarded_download_dry_run_commits_without_staging_or_state_mutation() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let mut state = StateFile::default();

        let decision = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"remote"),
            &expected,
            ExecutionMode::DryRun,
            &PanicGate,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Committed);
        assert_eq!(std::fs::read(&target).unwrap(), b"local");
        assert!(!tmp_path(&target).exists());
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_download_cancellation_after_staging_preserves_destination_and_state() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        let tmp = tmp_path(&target);
        std::fs::write(&target, b"original local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let mut state = StateFile::default();
        let original_record = old_record();
        state
            .files
            .insert("page.txt".into(), original_record.clone());
        let gate = CancelAfterStaging {
            tmp: tmp.clone(),
            saw_temp: AtomicBool::new(false),
        };

        let decision = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Cancelled);
        assert!(gate.saw_temp.load(Ordering::SeqCst));
        assert_eq!(std::fs::read(&target).unwrap(), b"original local");
        assert_eq!(state.files.get("page.txt"), Some(&original_record));
        assert!(!tmp.exists());
    }

    #[test]
    fn guarded_download_fails_when_the_parent_disappears_without_recreating_it() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("nested");
        let target = parent.join("page.txt");
        std::fs::create_dir(&parent).unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        std::fs::remove_dir(&parent).unwrap();
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "nested/page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &UnconditionalCommitGate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("parent"));
        assert!(!parent.exists());
        assert!(!tmp_path(&target).exists());
        assert!(state.files.is_empty());
    }

    #[test]
    fn guarded_download_rejects_destination_identity_change_inside_claim() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"same bytes").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let changed = target.clone();
        let gate = HookGate::before(move || {
            std::fs::remove_file(&changed).unwrap();
            std::fs::write(&changed, b"same bytes").unwrap();
        });
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("destination changed"));
        assert_eq!(std::fs::read(&target).unwrap(), b"same bytes");
        assert!(state.files.is_empty());
        assert!(!tmp_path(&target).exists());
    }

    #[test]
    fn guarded_download_rejects_destination_type_change_inside_claim() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let changed = target.clone();
        let gate = HookGate::before(move || {
            std::fs::remove_file(&changed).unwrap();
            std::fs::create_dir(&changed).unwrap();
        });
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("destination changed"));
        assert!(target.is_dir());
        assert!(state.files.is_empty());
        assert!(!tmp_path(&target).exists());
    }

    #[test]
    fn guarded_download_rename_failure_cleans_temp_without_updating_state() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        let tmp = tmp_path(&target);
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let removed_tmp = tmp.clone();
        let gate = HookGate::before(move || std::fs::remove_file(removed_tmp).unwrap());
        let mut state = StateFile::default();

        let error = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &remote(b"new remote"),
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("renaming"));
        assert_eq!(std::fs::read(&target).unwrap(), b"local");
        assert!(state.files.is_empty());
        assert!(!tmp.exists());
    }

    #[test]
    fn guarded_download_claim_can_commit_before_late_invalidation() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("page.txt");
        std::fs::write(&target, b"local").unwrap();
        let expected = ExpectedLocalDestination::capture(root.path(), &target).unwrap();
        let invalidated = target.clone();
        let gate = HookGate::after(move || std::fs::write(invalidated, b"late edit").unwrap());
        let mut state = StateFile::default();
        let expected_remote = remote(b"new remote");

        let decision = download_one_guarded(
            &mut state,
            &target,
            "page.txt",
            &expected_remote,
            &expected,
            ExecutionMode::Apply,
            &gate,
        )
        .unwrap();

        assert_eq!(decision, CommitDecision::Committed);
        assert_eq!(std::fs::read(&target).unwrap(), b"late edit");
        assert_eq!(state.files["page.txt"].sha256, expected_remote.sha256);
        assert!(!tmp_path(&target).exists());
    }

    #[test]
    fn staging_preserves_a_preexisting_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("page.txt");
        let tmp = tmp_path(&target);
        let original_target = b"original target";
        let original_tmp = b"another writer's staged bytes";
        std::fs::write(&target, original_target).unwrap();
        std::fs::write(&tmp, original_tmp).unwrap();

        let rejected = match stage_local_write(&target, b"replacement") {
            Ok(staged) => {
                drop(staged);
                false
            }
            Err(_) => true,
        };

        assert!(rejected);
        assert_eq!(std::fs::read(&target).unwrap(), original_target);
        assert_eq!(std::fs::read(&tmp).unwrap(), original_tmp);
    }

    #[test]
    fn staging_rejects_a_second_writer_without_changing_first_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("page.txt");
        let tmp = tmp_path(&target);
        let first_bytes = b"first writer";

        let first = stage_local_write(&target, first_bytes).unwrap();
        let second_rejected = match stage_local_write(&target, b"second writer") {
            Ok(staged) => {
                drop(staged);
                false
            }
            Err(_) => true,
        };

        assert!(second_rejected);
        assert_eq!(std::fs::read(&tmp).unwrap(), first_bytes);
        drop(first);
        assert!(!tmp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn staging_preserves_a_preexisting_temp_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("page.txt");
        let tmp = tmp_path(&target);
        let symlink_target = dir.path().join("do-not-touch.txt");
        let original = b"unrelated bytes";
        std::fs::write(&symlink_target, original).unwrap();
        symlink(&symlink_target, &tmp).unwrap();

        let rejected = match stage_local_write(&target, b"replacement") {
            Ok(staged) => {
                drop(staged);
                false
            }
            Err(_) => true,
        };

        assert!(rejected);
        assert!(
            std::fs::symlink_metadata(&tmp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&symlink_target).unwrap(), original);
        assert!(!target.exists());
    }
}
