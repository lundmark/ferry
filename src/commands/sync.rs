//! `ferry sync` — bidirectional reconciliation in a single pass.
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

pub use self::commit::{CommitDecision, CommitGate, UnconditionalCommitGate};
use self::scope::SyncScope;
use crate::commands::file_transfer::{RemoteDestinationSnapshot, RemoteWrite};
use crate::commands::pull::{ExpectedLocalDestination, download_one, download_one_guarded};
use crate::commands::push::{
    ExpectedLocalSource, ExpectedRemoteDestination, upload_one, upload_one_guarded,
};
use crate::commands::remote_hash::{self, RemoteFileRetrieval, RemoteHash};
use crate::commands::walk::{remote_join, walk_local, walk_remote};
use crate::commands::{ExecutionMode, state_path_for};
use crate::config::Config;
use crate::ftp::{Entry, Ftp, Remote, StrictRemote};
use crate::hash::{hash_bytes, hash_file};
use crate::ignored::Matcher;
use crate::state::{FileState, StateFile, classify};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// The scoped sync engine wires this collector in Task 5.
pub(crate) mod commit;
mod inventory;
#[cfg(test)]
mod production_tests;
pub use inventory::EntryKind;
pub mod scope;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEventKind {
    Unchanged,
    Uploaded,
    Downloaded,
    CreatedLocalDirectory,
    CreatedRemoteDirectory,
    SkippedAbsent,
    ForcedRemoteOverwrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEvent {
    pub path: String,
    pub kind: SyncEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncIssue {
    FileConflict {
        path: String,
        state: FileState,
    },
    TypeConflict {
        path: String,
        local: EntryKind,
        remote: EntryKind,
    },
}

impl SyncIssue {
    fn path(&self) -> &str {
        match self {
            Self::FileConflict { path, .. } | Self::TypeConflict { path, .. } => path,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub events: Vec<SyncEvent>,
    pub issues: Vec<SyncIssue>,
    pub cancelled: bool,
}

fn is_at_or_below_conflict(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn type_conflict_prefixes(entries: &BTreeMap<String, inventory::InventoryEntry>) -> Vec<String> {
    let mut prefixes: Vec<String> = Vec::new();
    for (path, entry) in entries {
        let is_conflict = matches!(
            (entry.local, entry.remote),
            (Some(local), Some(remote)) if local != remote
        );
        if is_conflict
            && !prefixes
                .iter()
                .any(|prefix| is_at_or_below_conflict(path, prefix))
        {
            prefixes.push(path.clone());
        }
    }
    prefixes
}

fn classify_inventory_shapes(
    entries: BTreeMap<String, inventory::InventoryEntry>,
) -> (Vec<String>, SyncOutcome) {
    let conflict_prefixes = type_conflict_prefixes(&entries);
    let mut files = Vec::new();
    let mut outcome = SyncOutcome::default();

    for prefix in &conflict_prefixes {
        let entry = entries
            .get(prefix)
            .expect("type-conflict prefix came from inventory");
        let (Some(local), Some(remote)) = (entry.local, entry.remote) else {
            unreachable!("type conflicts require entries on both sides");
        };
        outcome.issues.push(SyncIssue::TypeConflict {
            path: prefix.clone(),
            local,
            remote,
        });
    }

    for (path, entry) in entries {
        if conflict_prefixes
            .iter()
            .any(|prefix| is_at_or_below_conflict(&path, prefix))
        {
            continue;
        }
        match (entry.local, entry.remote) {
            (None, None) if entry.in_state => outcome.events.push(SyncEvent {
                path,
                kind: SyncEventKind::SkippedAbsent,
            }),
            (Some(EntryKind::File), Some(EntryKind::File))
            | (Some(EntryKind::File), None)
            | (None, Some(EntryKind::File)) => files.push(path),
            (Some(EntryKind::Directory), Some(EntryKind::Directory))
            | (Some(EntryKind::Directory), None)
            | (None, Some(EntryKind::Directory))
            | (None, None) => {}
            (Some(_), Some(_)) => {
                unreachable!("all type conflicts were suppressed by their prefix")
            }
        }
    }
    (files, outcome)
}

#[derive(Debug)]
struct ScheduledAction {
    path: String,
    kind: SyncEventKind,
}

fn execute_structured_plan(
    actions: Vec<ScheduledAction>,
    mut outcome: SyncOutcome,
    gate: &dyn CommitGate,
    mut execute: impl FnMut(&ScheduledAction) -> Result<CommitDecision>,
) -> Result<SyncOutcome> {
    for action in &actions {
        if !gate.is_current() {
            outcome.cancelled = true;
            return Ok(outcome);
        }
        match execute(action)? {
            CommitDecision::Committed => outcome.events.push(SyncEvent {
                path: action.path.clone(),
                kind: action.kind.clone(),
            }),
            CommitDecision::Cancelled => {
                outcome.cancelled = true;
                return Ok(outcome);
            }
        }
    }

    Ok(outcome)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExpectedLocalDirectory {
    Missing,
    Directory { canonical_in_root: PathBuf },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ExpectedDirectorySnapshots {
    pub relative: String,
    pub local: ExpectedLocalDirectory,
    pub remote: RemoteDestinationSnapshot,
}

#[derive(Debug)]
struct AuthoritativeUploadCandidate {
    state: FileState,
    destination: ExpectedRemoteDestination,
}

#[derive(Debug)]
struct PreparedTransfer {
    event: ScheduledAction,
    operation: PreparedOperation,
}

#[derive(Debug)]
enum PreparedOperation {
    Upload {
        remote_path: String,
        bytes: Vec<u8>,
        hash: String,
        source: ExpectedLocalSource,
        destination: ExpectedRemoteDestination,
    },
    Download {
        local_path: PathBuf,
        remote: RemoteHash,
        destination: ExpectedLocalDestination,
    },
}

/// Scoped sync transport adapter.
///
/// The legacy route intentionally keeps its historical `Ftp` implementations.
/// Scoped sync instead delegates every reachable FTP operation to a strict,
/// source-dropping method so hostile server replies cannot become user output.
struct ScopedFtp<'a> {
    inner: &'a mut Ftp,
}

impl Remote for ScopedFtp<'_> {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.inner.list_strict(dir)
    }

    fn file_size(&mut self, path: &str) -> Result<u64> {
        self.inner.size_scoped(path)
    }
}

impl StrictRemote for ScopedFtp<'_> {
    fn list_dir_strict(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.inner.list_strict(dir)
    }
}

impl RemoteFileRetrieval for ScopedFtp<'_> {
    fn mtime(&mut self, remote_path: &str) -> Result<chrono::DateTime<chrono::Utc>> {
        self.inner.mtime_scoped(remote_path)
    }

    fn size(&mut self, remote_path: &str) -> Result<u64> {
        self.inner.size_scoped(remote_path)
    }

    fn download(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        self.inner.download_scoped(remote_path)
    }
}

impl RemoteWrite for ScopedFtp<'_> {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.inner.upload_bytes_scoped(path, bytes)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.inner.rename_scoped(from, to)
    }

    fn rm(&mut self, path: &str) -> Result<()> {
        self.inner.rm_scoped(path)
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        self.inner.mkdir_scoped_strict(path)
    }

    fn mkdir_scoped_strict(&mut self, path: &str) -> Result<()> {
        self.inner.mkdir_scoped_strict(path)
    }

    fn mtime(&mut self, path: &str) -> Result<chrono::DateTime<chrono::Utc>> {
        self.inner.mtime_scoped(path)
    }

    fn destination_snapshot(
        &mut self,
        remote_root: &str,
        path: &str,
    ) -> Result<RemoteDestinationSnapshot> {
        <Ftp as RemoteWrite>::destination_snapshot(self.inner, remote_root, path)
    }
}

pub fn run_scoped(
    config_path: &Path,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<SyncOutcome> {
    if scope == SyncScope::LegacyProject {
        anyhow::bail!("scoped sync requires an explicit path");
    }

    let cfg = Config::load(config_path)?;
    let local_root = cfg.paths.local_root.clone();
    let state_path = state_path_for(&local_root, mode);
    let mut state = StateFile::load_or_default(&state_path)?;
    let initial_files = state.files.clone();
    let matcher = Matcher::new(&cfg.sync.ignore, &local_root)?;
    let mut ftp = Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )?;
    let mut remote = ScopedFtp { inner: &mut ftp };

    let execution = run_scoped_with(
        &mut remote,
        &mut state,
        &local_root,
        &cfg.paths.remote_root,
        &matcher,
        scope,
        force,
        mode,
        gate,
    );
    let should_save = state.files != initial_files;
    let save = if mode.should_apply() && should_save {
        state.save(&state_path)
    } else {
        Ok(())
    };

    match (execution, save) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(save_error)) => Err(error.context(format!(
            "also failed to save completed sync state: {save_error:#}"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_scoped_with<R>(
    remote: &mut R,
    state: &mut StateFile,
    local_root: &Path,
    remote_root: &str,
    matcher: &Matcher,
    scope: SyncScope,
    force: bool,
    mode: ExecutionMode,
    gate: &dyn CommitGate,
) -> Result<SyncOutcome>
where
    R: StrictRemote + RemoteFileRetrieval + RemoteWrite,
{
    let inventory = inventory::collect(remote, local_root, remote_root, matcher, state, scope)?;
    let entries = inventory.entries;
    let (file_paths, mut outcome) = classify_inventory_shapes(entries.clone());
    let directories = capture_directory_snapshots(local_root, &entries)?;

    let mut transfers = Vec::new();
    for relative in file_paths {
        let entry = entries
            .get(&relative)
            .expect("file path came from scoped inventory");
        let local_path = local_root.join(&relative);
        let remote_path = remote_join(remote_root, &relative);
        let local_hash = if entry.local == Some(EntryKind::File) {
            Some(
                hash_file(&local_path)
                    .with_context(|| format!("hashing local {}", local_path.display()))?,
            )
        } else {
            None
        };
        let remote_hash = if entry.remote == Some(EntryKind::File) {
            Some(remote_hash::compute_with(
                remote,
                state,
                &relative,
                &remote_path,
                true,
            )?)
        } else {
            None
        };
        let known = state
            .files
            .get(&relative)
            .map(|record| record.sha256.as_str());
        let preliminary_state = classify(
            local_hash.as_deref(),
            remote_hash.as_ref().map(|hash| hash.sha256.as_str()),
            known,
        );
        let mut upload_candidate = if matches!(
            preliminary_state,
            FileState::LocalChanged | FileState::LocalOnly
        ) || force
            && matches!(
                preliminary_state,
                FileState::BothChanged | FileState::Untracked
            ) {
            Some(capture_authoritative_upload_candidate(
                remote,
                remote_root,
                &remote_path,
                &relative,
                entry.remote,
                local_hash
                    .as_deref()
                    .expect("upload candidate has a local hash"),
                known,
            )?)
        } else {
            None
        };
        let file_state = upload_candidate
            .as_ref()
            .map_or(preliminary_state, |candidate| candidate.state);

        match file_state {
            FileState::InSync => outcome.events.push(SyncEvent {
                path: relative,
                kind: SyncEventKind::Unchanged,
            }),
            FileState::LocalChanged | FileState::LocalOnly => {
                let destination = upload_candidate
                    .take()
                    .expect("final upload state has an authoritative candidate")
                    .destination;
                transfers.push(prepare_upload(
                    local_root,
                    &local_path,
                    relative,
                    remote_path,
                    local_hash.expect("upload state has a local hash"),
                    destination,
                    SyncEventKind::Uploaded,
                )?);
            }
            FileState::RemoteChanged | FileState::RemoteOnly => {
                transfers.push(prepare_download(
                    remote,
                    local_root,
                    &local_path,
                    local_hash.as_deref(),
                    relative,
                    remote_path,
                    remote_hash.expect("download state has a remote hash"),
                )?);
            }
            FileState::BothChanged | FileState::Untracked if force => {
                let destination = upload_candidate
                    .take()
                    .expect("final forced upload state has an authoritative candidate")
                    .destination;
                transfers.push(prepare_upload(
                    local_root,
                    &local_path,
                    relative,
                    remote_path,
                    local_hash.expect("forced upload has a local hash"),
                    destination,
                    SyncEventKind::ForcedRemoteOverwrite,
                )?);
            }
            FileState::BothChanged | FileState::Untracked => {
                outcome.issues.push(SyncIssue::FileConflict {
                    path: relative,
                    state: file_state,
                });
            }
        }
    }

    let scheduled = transfers
        .iter()
        .map(|transfer| ScheduledAction {
            path: transfer.event.path.clone(),
            kind: transfer.event.kind.clone(),
        })
        .collect();
    let mut transfers = transfers.into_iter();
    outcome = execute_structured_plan(scheduled, outcome, gate, |_action| {
        let transfer = transfers
            .next()
            .expect("scheduled action has a prepared transfer");
        match transfer.operation {
            PreparedOperation::Upload {
                remote_path,
                bytes,
                hash,
                source,
                destination,
            } => upload_one_guarded(
                remote,
                state,
                &transfer.event.path,
                remote_root,
                &remote_path,
                &bytes,
                &hash,
                &source,
                &destination,
                mode,
                gate,
            ),
            PreparedOperation::Download {
                local_path,
                remote,
                destination,
            } => download_one_guarded(
                state,
                &local_path,
                &transfer.event.path,
                &remote,
                &destination,
                mode,
                gate,
            ),
        }
    })?;
    if outcome.cancelled {
        sort_outcome(&mut outcome);
        return Ok(outcome);
    }

    if !gate.is_current() {
        outcome.cancelled = true;
        sort_outcome(&mut outcome);
        return Ok(outcome);
    }
    for expected in &directories {
        if !gate.is_current() {
            outcome.cancelled = true;
            sort_outcome(&mut outcome);
            return Ok(outcome);
        }
        validate_directory_snapshot(remote, local_root, remote_root, expected)?;
    }
    if !gate.is_current() {
        outcome.cancelled = true;
    }
    sort_outcome(&mut outcome);
    Ok(outcome)
}

fn prepare_upload(
    local_root: &Path,
    local_path: &Path,
    relative: String,
    remote_path: String,
    local_hash: String,
    destination: ExpectedRemoteDestination,
    kind: SyncEventKind,
) -> Result<PreparedTransfer> {
    let source = ExpectedLocalSource::capture(local_root, local_path)?;
    let bytes = std::fs::read(&source.path)
        .with_context(|| format!("reading local {}", source.path.display()))?;
    if hash_bytes(&bytes) != local_hash {
        anyhow::bail!("local source changed while planning {relative}");
    }
    Ok(PreparedTransfer {
        event: ScheduledAction {
            path: relative,
            kind,
        },
        operation: PreparedOperation::Upload {
            remote_path,
            bytes,
            hash: local_hash,
            source,
            destination,
        },
    })
}

fn capture_authoritative_upload_candidate<R: RemoteWrite>(
    remote: &mut R,
    remote_root: &str,
    remote_path: &str,
    relative: &str,
    inventory_remote: Option<EntryKind>,
    local_hash: &str,
    known: Option<&str>,
) -> Result<AuthoritativeUploadCandidate> {
    let snapshot = remote.destination_snapshot(remote_root, remote_path)?;
    let state = match (inventory_remote, &snapshot) {
        (
            Some(EntryKind::File),
            RemoteDestinationSnapshot::File {
                sha256,
                size: _,
                modified: _,
            },
        ) => classify(Some(local_hash), Some(sha256), known),
        (Some(EntryKind::File), RemoteDestinationSnapshot::Missing) => {
            anyhow::bail!("remote file disappeared while planning {relative}")
        }
        (Some(EntryKind::File), RemoteDestinationSnapshot::Directory) => {
            anyhow::bail!("remote file became a directory while planning {relative}")
        }
        (None, RemoteDestinationSnapshot::Missing) => classify(Some(local_hash), None, known),
        (None, RemoteDestinationSnapshot::File { .. }) => {
            anyhow::bail!("remote file appeared while planning {relative}")
        }
        (None, RemoteDestinationSnapshot::Directory) => {
            anyhow::bail!("remote directory appeared while planning {relative}")
        }
        (Some(EntryKind::Directory), _) => {
            anyhow::bail!("remote directory is not an upload candidate at {relative}")
        }
    };
    Ok(AuthoritativeUploadCandidate {
        state,
        destination: ExpectedRemoteDestination { snapshot },
    })
}

fn prepare_download<R: RemoteFileRetrieval>(
    remote: &mut R,
    local_root: &Path,
    local_path: &Path,
    expected_local_hash: Option<&str>,
    relative: String,
    remote_path: String,
    remote_hash: RemoteHash,
) -> Result<PreparedTransfer> {
    let expected_remote = remote_snapshot(&remote_hash);
    let remote_hash = remote_hash::complete_for_install(remote, &remote_path, remote_hash)?;
    if remote_snapshot(&remote_hash) != expected_remote {
        anyhow::bail!("remote source changed while planning {relative}");
    }
    let destination = ExpectedLocalDestination::capture(local_root, local_path)?;
    verify_planned_local_hash(local_path, expected_local_hash)?;
    Ok(PreparedTransfer {
        event: ScheduledAction {
            path: relative,
            kind: SyncEventKind::Downloaded,
        },
        operation: PreparedOperation::Download {
            local_path: local_path.to_path_buf(),
            remote: remote_hash,
            destination,
        },
    })
}

fn verify_planned_local_hash(path: &Path, expected: Option<&str>) -> Result<()> {
    let current = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading local destination {}", path.display()));
        }
        Ok(_) => {
            let metadata = std::fs::metadata(path)
                .with_context(|| format!("reading local destination {}", path.display()))?;
            if !metadata.is_file() {
                anyhow::bail!("local destination changed at {}", path.display());
            }
            Some(
                hash_file(path)
                    .with_context(|| format!("hashing local destination {}", path.display()))?,
            )
        }
    };
    if current.as_deref() != expected {
        anyhow::bail!(
            "local destination changed while planning {}",
            path.display()
        );
    }
    Ok(())
}

fn remote_snapshot(hash: &RemoteHash) -> RemoteDestinationSnapshot {
    RemoteDestinationSnapshot::File {
        size: hash.size,
        modified: hash.mtime,
        sha256: hash.sha256.clone(),
    }
}

fn capture_directory_snapshots(
    local_root: &Path,
    entries: &BTreeMap<String, inventory::InventoryEntry>,
) -> Result<Vec<ExpectedDirectorySnapshots>> {
    let canonical_root = local_root
        .canonicalize()
        .with_context(|| format!("canonicalizing local_root {}", local_root.display()))?;
    let mut snapshots = Vec::new();
    let conflict_prefixes = type_conflict_prefixes(entries);
    for (relative, entry) in entries {
        if conflict_prefixes
            .iter()
            .any(|prefix| is_at_or_below_conflict(relative, prefix))
        {
            continue;
        }
        let is_directory = matches!(
            (entry.local, entry.remote),
            (Some(EntryKind::Directory), Some(EntryKind::Directory))
                | (Some(EntryKind::Directory), None)
                | (None, Some(EntryKind::Directory))
        );
        if !is_directory {
            continue;
        }
        let local = if entry.local == Some(EntryKind::Directory) {
            let path = local_root.join(relative);
            let canonical_in_root = path
                .canonicalize()
                .with_context(|| format!("canonicalizing local directory {}", path.display()))?;
            if !canonical_in_root.starts_with(&canonical_root) {
                anyhow::bail!(
                    "local directory {} resolves outside local_root {}",
                    path.display(),
                    local_root.display()
                );
            }
            ExpectedLocalDirectory::Directory { canonical_in_root }
        } else {
            ExpectedLocalDirectory::Missing
        };
        let remote = if entry.remote == Some(EntryKind::Directory) {
            RemoteDestinationSnapshot::Directory
        } else {
            RemoteDestinationSnapshot::Missing
        };
        snapshots.push(ExpectedDirectorySnapshots {
            relative: relative.clone(),
            local,
            remote,
        });
    }
    Ok(snapshots)
}

fn validate_directory_snapshot<R: RemoteWrite>(
    remote: &mut R,
    local_root: &Path,
    remote_root: &str,
    expected: &ExpectedDirectorySnapshots,
) -> Result<()> {
    let local_path = local_root.join(&expected.relative);
    match &expected.local {
        ExpectedLocalDirectory::Missing => match std::fs::symlink_metadata(&local_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => anyhow::bail!(
                "local directory destination appeared at {}",
                local_path.display()
            ),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading local directory {}", local_path.display()));
            }
        },
        ExpectedLocalDirectory::Directory { canonical_in_root } => {
            let metadata = std::fs::metadata(&local_path)
                .with_context(|| format!("reading local directory {}", local_path.display()))?;
            let canonical = local_path.canonicalize().with_context(|| {
                format!("canonicalizing local directory {}", local_path.display())
            })?;
            if !metadata.is_dir() || canonical != *canonical_in_root {
                anyhow::bail!("local directory changed at {}", local_path.display());
            }
        }
    }

    let remote_path = remote_join(remote_root, &expected.relative);
    let current = remote.destination_snapshot(remote_root, &remote_path)?;
    if current != expected.remote {
        anyhow::bail!(
            "remote directory changed during scoped sync at {:?}",
            expected.relative
        );
    }
    Ok(())
}

fn sort_outcome(outcome: &mut SyncOutcome) {
    outcome
        .events
        .sort_by(|left, right| left.path.cmp(&right.path));
    outcome
        .issues
        .sort_by(|left, right| left.path().cmp(right.path()));
}

fn outcome_error(outcome: &SyncOutcome) -> Result<Option<anyhow::Error>> {
    if let Some(SyncIssue::TypeConflict {
        path,
        local,
        remote,
    }) = outcome
        .issues
        .iter()
        .find(|issue| matches!(issue, SyncIssue::TypeConflict { .. }))
    {
        return Ok(Some(anyhow::anyhow!(
            "sync aborted: type conflict at {path} ({local:?} locally, {remote:?} remotely)"
        )));
    }
    if outcome.cancelled {
        return Ok(Some(anyhow::anyhow!(
            "sync cancelled because the selected scope changed; retry"
        )));
    }
    if outcome
        .issues
        .iter()
        .any(|issue| matches!(issue, SyncIssue::FileConflict { .. }))
    {
        return Ok(Some(
            crate::error::Exit::Conflict(
                "sync aborted: one or more files diverged on both sides (use --force to take local)"
                    .into(),
            )
            .into(),
        ));
    }
    Ok(None)
}

fn resolve_scoped_cli_path(config_path: &Path, input: &str) -> Result<SyncScope> {
    let cfg = Config::load(config_path)?;
    let scope = scope::from_cli_path(&cfg.paths.local_root, Some(input))?;
    if scope == SyncScope::LegacyProject {
        anyhow::bail!("explicit sync path resolved to legacy project scope");
    }
    Ok(scope)
}

fn render_outcome(outcome: &SyncOutcome, mode: ExecutionMode) -> Result<()> {
    for event in &outcome.events {
        match event.kind {
            SyncEventKind::Unchanged => {}
            SyncEventKind::Uploaded => {
                if mode.is_dry_run() {
                    println!("would upload {}", event.path);
                } else {
                    println!("uploaded {}", event.path);
                }
            }
            SyncEventKind::Downloaded => {
                if mode.is_dry_run() {
                    println!("would download {}", event.path);
                } else {
                    println!("downloaded {}", event.path);
                }
            }
            SyncEventKind::CreatedLocalDirectory => {
                if mode.is_dry_run() {
                    println!("would create local directory {}", event.path);
                } else {
                    println!("created local directory {}", event.path);
                }
            }
            SyncEventKind::CreatedRemoteDirectory => {
                if mode.is_dry_run() {
                    println!("would create remote directory {}", event.path);
                } else {
                    println!("created remote directory {}", event.path);
                }
            }
            SyncEventKind::SkippedAbsent => {
                eprintln!("skip (not on local or remote): {}", event.path);
            }
            SyncEventKind::ForcedRemoteOverwrite => {
                if mode.is_dry_run() {
                    eprintln!(
                        "would overwrite remote with local (--force): {}",
                        event.path
                    );
                } else {
                    eprintln!("overwriting remote with local (--force): {}", event.path);
                }
            }
        }
    }

    for issue in &outcome.issues {
        match issue {
            SyncIssue::FileConflict { path, state } => eprintln!(
                "conflict ({state:?}, local and remote diverged): {path} — pass --force to take local"
            ),
            SyncIssue::TypeConflict {
                path,
                local,
                remote,
            } => eprintln!("type conflict: {path} is {local:?} locally and {remote:?} remotely"),
        }
    }

    if let Some(error) = outcome_error(outcome)? {
        return Err(error);
    }
    Ok(())
}

pub fn run_cli(
    config_path: &Path,
    path: Option<&str>,
    select: bool,
    force: bool,
    mode: ExecutionMode,
) -> Result<()> {
    if path.is_none() && !select {
        return run_legacy(config_path, force, mode);
    }
    if select {
        anyhow::bail!("interactive path selection is not implemented yet");
    }
    let input = path.expect("non-select scoped sync has a path");
    let scope = resolve_scoped_cli_path(config_path, input)?;
    let outcome = run_scoped(config_path, scope, force, mode, &UnconditionalCommitGate)?;
    render_outcome(&outcome, mode)
}

fn run_legacy(config_path: &Path, force: bool, mode: ExecutionMode) -> Result<()> {
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
        // MDTM/SIZE fast path: if the cached (mtime, size) match, we trust
        // the cached hash and skip the download. When the fast path can't
        // fire we ask for bytes so we have them for the download branch.
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
                // Nothing to do; both sides agree.
            }
            FileState::LocalChanged | FileState::LocalOnly => {
                let bytes = std::fs::read(local_root.join(rel))
                    .with_context(|| format!("reading local {}", local_root.join(rel).display()))?;
                let new_hash = local_hash
                    .as_deref()
                    .expect("local_hash set when on_local is true");
                upload_one(
                    &mut ftp,
                    &mut state,
                    rel,
                    &remote_path,
                    &bytes,
                    new_hash,
                    mode,
                )?;
                if mode.is_dry_run() {
                    println!("would upload {rel}");
                } else {
                    println!("uploaded {rel}");
                }
            }
            FileState::RemoteChanged | FileState::RemoteOnly => {
                // We need real bytes to write locally. If the fast path
                // fired (rh.bytes is None) we would normally have classified
                // as InSync (since the cached hash matches state). Defensive
                // fallback: fetch fresh if bytes are absent.
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
                if mode.is_dry_run() {
                    println!("would download {rel}");
                } else {
                    println!("downloaded {rel}");
                }
            }
            FileState::BothChanged | FileState::Untracked => {
                // Conflict: both sides moved away from the last known state
                // (or there is no known state and both sides have something).
                // Refuse unless --force, in which case the design says local
                // wins — sync's "force" is the user telling us to just push
                // their working copy as the canonical version.
                if force {
                    let bytes = std::fs::read(local_root.join(rel)).with_context(|| {
                        format!("reading local {}", local_root.join(rel).display())
                    })?;
                    let new_hash = local_hash
                        .as_deref()
                        .expect("local_hash set when on_local is true");
                    if mode.is_dry_run() {
                        eprintln!("would overwrite remote with local (--force): {rel}");
                    } else {
                        eprintln!("overwriting remote with local (--force): {rel}");
                    }
                    upload_one(
                        &mut ftp,
                        &mut state,
                        rel,
                        &remote_path,
                        &bytes,
                        new_hash,
                        mode,
                    )?;
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

    // Persist apply progress even on conflict so the clean files don't have to be
    // re-hashed next run. Matches push/pull behavior.
    if mode.should_apply() {
        state.save(&state_path)?;
    }

    if had_conflict {
        // Tag as `Exit::Conflict` so `main()` returns exit code 2 — Zed's
        // tasks.json uses that to surface a "needs --force" message rather
        // than a generic failure.
        return Err(crate::error::Exit::Conflict(
            "sync aborted: one or more files diverged on both sides (use --force to take local)"
                .into(),
        )
        .into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::commit::CommitGate;
    use super::{SyncIssue, SyncOutcome};
    use crate::state::FileState;
    use anyhow::Result;

    #[test]
    fn structured_run_scoped_exposes_the_guarded_structured_api() {
        let _: fn(
            &std::path::Path,
            super::scope::SyncScope,
            bool,
            crate::commands::ExecutionMode,
            &dyn CommitGate,
        ) -> Result<SyncOutcome> = super::run_scoped;
    }

    #[test]
    fn structured_cli_file_conflicts_map_to_conflict_exit() {
        let outcome = SyncOutcome {
            issues: vec![SyncIssue::FileConflict {
                path: "conflict.c".into(),
                state: FileState::BothChanged,
            }],
            ..SyncOutcome::default()
        };

        let error = super::outcome_error(&outcome).unwrap().unwrap();
        assert!(matches!(
            error.downcast_ref::<crate::error::Exit>(),
            Some(crate::error::Exit::Conflict(_))
        ));
    }

    #[test]
    fn structured_cli_type_conflicts_map_to_generic_errors() {
        let outcome = SyncOutcome {
            issues: vec![SyncIssue::TypeConflict {
                path: "type.c".into(),
                local: super::EntryKind::File,
                remote: super::EntryKind::Directory,
            }],
            ..SyncOutcome::default()
        };

        let error = super::outcome_error(&outcome).unwrap().unwrap();
        assert!(error.downcast_ref::<crate::error::Exit>().is_none());
        assert!(format!("{error:#}").contains("type conflict"));
    }

    #[test]
    fn structured_cli_explicit_paths_never_resolve_to_legacy_project() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(crate::names::CONFIG_FILE),
            r#"
[connection]
host = "example.invalid"
user = "u"
password = "p"
[paths]
local_root = "."
remote_root = "/remote"
"#,
        )
        .unwrap();
        let config = project.path().join(crate::names::CONFIG_FILE);

        assert_eq!(
            super::resolve_scoped_cli_path(&config, ".").unwrap(),
            super::scope::SyncScope::RootDirectory
        );
        assert_eq!(
            super::resolve_scoped_cli_path(&config, "file.c").unwrap(),
            super::scope::SyncScope::Path("file.c".into())
        );
    }

    #[test]
    fn structured_download_planning_rejects_local_hash_changes() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.c");
        std::fs::write(&target, b"current").unwrap();
        let current = crate::hash::hash_bytes(b"current");

        super::verify_planned_local_hash(&target, Some(&current)).unwrap();
        let error = super::verify_planned_local_hash(&target, Some("inventoried-old")).unwrap_err();
        assert!(format!("{error:#}").contains("local destination changed"));

        let missing = root.path().join("missing.c");
        super::verify_planned_local_hash(&missing, None).unwrap();
        assert!(super::verify_planned_local_hash(&missing, Some("expected-present")).is_err());
    }
}
