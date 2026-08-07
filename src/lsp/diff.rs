#![allow(dead_code)] // The protocol loop wires this runtime in the next change.

use std::ffi::OsStr;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, ensure};
use fs2::FileExt;
use tempfile::{Builder, NamedTempFile, TempDir};

use super::document_state::OperationGuard;

const ROOT_PREFIX: &str = "ferry-lsp-diff-v1-";
const SNAPSHOT_PREFIX: &str = "remote-";
const LOCK_NAME: &str = ".lock";
const MAX_EXTENSION_BYTES: usize = 32;

pub(crate) trait DiffLauncher: Send {
    fn launch(&mut self, local: &Path, remote: &Path) -> Result<()>;
}

pub(crate) struct ZedDiffLauncher;

impl DiffLauncher for ZedDiffLauncher {
    fn launch(&mut self, local: &Path, remote: &Path) -> Result<()> {
        zed_diff_command(local, remote)
            .spawn()
            .context("launching Zed native diff")?;
        Ok(())
    }
}

fn zed_diff_command(local: &Path, remote: &Path) -> Command {
    let mut command = Command::new("zed");
    command.arg("--diff").arg(local).arg(remote);
    command
}

#[derive(Clone)]
pub(crate) struct SharedSnapshotStore {
    inner: Arc<SnapshotInner>,
}

#[derive(Clone)]
pub(crate) struct SnapshotShutdown {
    inner: Arc<SnapshotInner>,
}

struct SnapshotInner {
    state: Mutex<SnapshotState>,
}

struct SnapshotState {
    closed: bool,
    retained: Vec<PreparedSnapshot>,
    lock_file: Option<File>,
    root: Option<TempDir>,
}

pub(crate) struct PreparedSnapshot {
    file: NamedTempFile,
}

impl PreparedSnapshot {
    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }
}

impl SnapshotInner {
    fn lock(&self) -> Result<MutexGuard<'_, SnapshotState>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("private snapshot store state is unavailable after a panic"))
    }
}

impl SharedSnapshotStore {
    pub(crate) fn new() -> Result<Self> {
        Self::new_in(&std::env::temp_dir())
    }

    fn new_in(temp_base: &Path) -> Result<Self> {
        let base = fs::canonicalize(temp_base).with_context(|| {
            format!(
                "resolving private snapshot temporary base {}",
                temp_base.display()
            )
        })?;
        let base_metadata = fs::symlink_metadata(&base).with_context(|| {
            format!(
                "inspecting private snapshot temporary base {}",
                base.display()
            )
        })?;
        ensure!(
            base_metadata.file_type().is_dir() && !base_metadata.file_type().is_symlink(),
            "private snapshot temporary base is not a directory"
        );

        cleanup_stale_roots(&base)?;

        let root = Builder::new()
            .prefix(ROOT_PREFIX)
            .tempdir_in(&base)
            .context("creating private snapshot root")?;
        let lock_path = root.path().join(LOCK_NAME);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .context("creating private snapshot root lock")?;
        FileExt::lock_exclusive(&lock_file).context("locking private snapshot root")?;

        Ok(Self {
            inner: Arc::new(SnapshotInner {
                state: Mutex::new(SnapshotState {
                    closed: false,
                    retained: Vec::new(),
                    lock_file: Some(lock_file),
                    root: Some(root),
                }),
            }),
        })
    }

    pub(crate) fn shutdown_handle(&self) -> SnapshotShutdown {
        SnapshotShutdown {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(crate) fn prepare_snapshot(
        &self,
        source_name: &Path,
        remote_bytes: &[u8],
    ) -> Result<PreparedSnapshot> {
        let root = {
            let state = self.inner.lock()?;
            ensure!(!state.closed, "private snapshot store is closed");
            state
                .root
                .as_ref()
                .map(|root| root.path().to_path_buf())
                .context("private snapshot root is unavailable")?
        };

        let suffix = safe_snapshot_suffix(source_name);
        let mut builder = Builder::new();
        builder.prefix(SNAPSHOT_PREFIX);
        if let Some(suffix) = &suffix {
            builder.suffix(suffix);
        }
        let mut file = builder
            .tempfile_in(&root)
            .context("creating private remote snapshot")?;
        file.write_all(remote_bytes)
            .context("writing private remote snapshot")?;
        file.flush().context("flushing private remote snapshot")?;
        make_snapshot_read_only(file.as_file())?;

        let prepared = PreparedSnapshot { file };

        {
            let state = self.inner.lock()?;
            ensure!(
                !state.closed,
                "private snapshot store closed during creation"
            );
            let active_root = state
                .root
                .as_ref()
                .context("private snapshot root is unavailable")?
                .path();
            ensure!(
                active_root == root,
                "private snapshot root changed during creation"
            );
            validate_active_root(active_root)?;
            validate_prepared_snapshot(active_root, &prepared)?;
        }

        Ok(prepared)
    }

    pub(crate) fn launch_and_retain<L>(
        &self,
        local: &Path,
        snapshot: PreparedSnapshot,
        guard: OperationGuard,
        launcher: &mut L,
    ) -> Result<()>
    where
        L: DiffLauncher + ?Sized,
    {
        let mut state = self.inner.lock()?;
        ensure!(!state.closed, "private snapshot store is closed");
        let root = state
            .root
            .as_ref()
            .context("private snapshot root is unavailable")?
            .path()
            .to_path_buf();

        ensure!(local.is_absolute(), "local diff path must be absolute");
        validate_active_root(&root)?;
        validate_prepared_snapshot(&root, &snapshot)?;
        ensure!(
            guard.try_claim(),
            "diff operation was cancelled before launch"
        );

        launcher
            .launch(local, snapshot.path())
            .context("launching Zed native diff")?;
        state.retained.push(snapshot);
        Ok(())
    }

    #[cfg(test)]
    fn root_path(&self) -> Result<PathBuf> {
        let state = self.inner.lock()?;
        state
            .root
            .as_ref()
            .map(|root| root.path().to_path_buf())
            .context("private snapshot root is unavailable")
    }
}

impl SnapshotShutdown {
    pub(crate) fn shutdown(&self) -> Result<()> {
        let root = {
            let mut state = self.inner.lock()?;
            if state.closed {
                return Ok(());
            }

            state.closed = true;
            state.retained.clear();
            let lock_file = state.lock_file.take();
            drop(lock_file);
            state.root.take()
        };

        if let Some(root) = root {
            let path = root.path().to_path_buf();
            root.close()
                .with_context(|| format!("removing private snapshot root {}", path.display()))?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn is_closed(&self) -> Result<bool> {
        Ok(self.inner.lock()?.closed)
    }
}

fn safe_snapshot_suffix(source_name: &Path) -> Option<String> {
    let extension = source_name.extension()?.to_str()?;
    if extension.is_empty()
        || extension.len() > MAX_EXTENSION_BYTES
        || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(format!(".{extension}"))
}

fn make_snapshot_read_only(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = file
            .metadata()
            .context("inspecting private remote snapshot permissions")?
            .permissions();
        let mode = permissions.mode();
        permissions.set_mode(mode & !0o222);
        file.set_permissions(permissions)
            .context("making private remote snapshot read-only")?;
    }

    #[cfg(not(unix))]
    let _ = file;

    Ok(())
}

fn validate_active_root(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root).context("inspecting active private snapshot root")?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "active private snapshot root is unsafe"
    );
    Ok(())
}

fn validate_prepared_snapshot(root: &Path, snapshot: &PreparedSnapshot) -> Result<()> {
    ensure!(
        snapshot.path().parent() == Some(root),
        "prepared snapshot is outside the active private root"
    );
    let metadata = fs::symlink_metadata(snapshot.path())
        .context("inspecting prepared private remote snapshot")?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "prepared snapshot is not a safe regular file"
    );
    Ok(())
}

fn cleanup_stale_roots(base: &Path) -> Result<()> {
    for entry in fs::read_dir(base).context("reading private snapshot temporary base")? {
        let Ok(entry) = entry else {
            continue;
        };
        if !is_recognizable_root_name(&entry.file_name()) {
            continue;
        }

        let candidate = entry.path();
        if candidate.parent() != Some(base) || candidate == base {
            continue;
        }
        let Ok(candidate_metadata) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        if !candidate_metadata.file_type().is_dir() || candidate_metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(resolved_candidate) = fs::canonicalize(&candidate) else {
            continue;
        };
        if resolved_candidate != candidate || resolved_candidate.parent() != Some(base) {
            continue;
        }

        let lock_path = candidate.join(LOCK_NAME);
        let Ok(lock_metadata) = fs::symlink_metadata(&lock_path) else {
            continue;
        };
        if !lock_metadata.file_type().is_file() || lock_metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(resolved_lock) = fs::canonicalize(&lock_path) else {
            continue;
        };
        if resolved_lock != lock_path || resolved_lock.parent() != Some(&candidate) {
            continue;
        }

        let Ok(lock_file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
            continue;
        };
        let Ok(opened_metadata) = lock_file.metadata() else {
            continue;
        };
        if !opened_metadata.file_type().is_file()
            || !same_file_identity(&lock_metadata, &opened_metadata)
        {
            continue;
        }
        match FileExt::try_lock_exclusive(&lock_file) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
            Err(_) => continue,
        }

        if !revalidate_stale_candidate(
            base,
            &candidate,
            &resolved_candidate,
            &lock_path,
            &lock_file,
        ) {
            continue;
        }

        fs::remove_dir_all(&candidate).with_context(|| {
            format!(
                "removing stale private snapshot root {}",
                candidate.display()
            )
        })?;
    }
    Ok(())
}

fn revalidate_stale_candidate(
    base: &Path,
    candidate: &Path,
    expected_resolved_candidate: &Path,
    lock_path: &Path,
    lock_file: &File,
) -> bool {
    if candidate == base || candidate.parent() != Some(base) {
        return false;
    }

    let Ok(candidate_metadata) = fs::symlink_metadata(candidate) else {
        return false;
    };
    if !candidate_metadata.file_type().is_dir() || candidate_metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(resolved_candidate) = fs::canonicalize(candidate) else {
        return false;
    };
    if resolved_candidate != candidate
        || resolved_candidate != expected_resolved_candidate
        || resolved_candidate.parent() != Some(base)
    {
        return false;
    }

    let Ok(lock_metadata) = fs::symlink_metadata(lock_path) else {
        return false;
    };
    if !lock_metadata.file_type().is_file() || lock_metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(resolved_lock) = fs::canonicalize(lock_path) else {
        return false;
    };
    if resolved_lock != lock_path || resolved_lock.parent() != Some(candidate) {
        return false;
    }

    lock_file
        .metadata()
        .is_ok_and(|opened_metadata| same_file_identity(&lock_metadata, &opened_metadata))
}

fn is_recognizable_root_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(suffix) = name.strip_prefix(ROOT_PREFIX) else {
        return false;
    };
    !suffix.is_empty()
        && suffix.len() <= 64
        && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(unix)]
fn same_file_identity(path_metadata: &Metadata, opened_metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    path_metadata.dev() == opened_metadata.dev() && path_metadata.ino() == opened_metadata.ino()
}

#[cfg(not(unix))]
fn same_file_identity(path_metadata: &Metadata, opened_metadata: &Metadata) -> bool {
    path_metadata.file_type().is_file() && opened_metadata.file_type().is_file()
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::ffi::{OsStr, OsString};
    use std::fmt;
    use std::fs::{self, File, OpenOptions};
    use std::path::{Path, PathBuf};

    use fs2::FileExt;
    use tempfile::{TempDir, tempdir};

    use super::{
        DiffLauncher, ROOT_PREFIX, SharedSnapshotStore, SnapshotShutdown, zed_diff_command,
    };
    use crate::lsp::document_state::{DocumentTracker, OperationGuard};

    #[derive(Default)]
    struct RecordingLauncher {
        calls: Vec<(PathBuf, PathBuf)>,
        fail: bool,
        guard_probe: Option<OperationGuard>,
        observed_claim_before_launch: bool,
    }

    impl RecordingLauncher {
        fn failing() -> Self {
            Self {
                fail: true,
                ..Self::default()
            }
        }

        fn probing(guard: OperationGuard) -> Self {
            Self {
                guard_probe: Some(guard),
                ..Self::default()
            }
        }
    }

    #[derive(Debug)]
    struct LauncherFailure;

    impl fmt::Display for LauncherFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("launcher sentinel")
        }
    }

    impl Error for LauncherFailure {}

    impl DiffLauncher for RecordingLauncher {
        fn launch(&mut self, local: &Path, remote: &Path) -> anyhow::Result<()> {
            if let Some(guard) = &self.guard_probe {
                self.observed_claim_before_launch = !guard.try_claim();
            }
            self.calls.push((local.to_path_buf(), remote.to_path_buf()));
            if self.fail {
                return Err(anyhow::Error::new(LauncherFailure));
            }
            Ok(())
        }
    }

    fn store_in(base: &TempDir) -> (SharedSnapshotStore, SnapshotShutdown) {
        let store = SharedSnapshotStore::new_in(base.path()).unwrap();
        let shutdown = store.shutdown_handle();
        (store, shutdown)
    }

    fn local_and_guard(directory: &Path) -> (PathBuf, DocumentTracker, OperationGuard) {
        let local = directory.join("local.rs");
        fs::write(&local, b"local bytes").unwrap();
        let mut tracker = DocumentTracker::default();
        tracker.open(local.clone(), "local bytes").unwrap();
        let guard = tracker.begin_clean_operation(&local).unwrap();
        (local, tracker, guard)
    }

    fn recognizable_root(base: &Path, suffix: &str) -> PathBuf {
        let root = base.join(format!("{ROOT_PREFIX}{suffix}"));
        fs::create_dir(&root).unwrap();
        fs::write(root.join(".lock"), b"").unwrap();
        root
    }

    #[test]
    fn snapshots_are_unique_and_preserve_only_safe_ascii_alphanumeric_extensions() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);

        let first = store
            .prepare_snapshot(Path::new("../../source.Component9"), b"first")
            .unwrap();
        let second = store
            .prepare_snapshot(Path::new("../../source.Component9"), b"second")
            .unwrap();
        let unsafe_extension = store
            .prepare_snapshot(Path::new("source.rś-1"), b"unsafe")
            .unwrap();
        let no_extension = store
            .prepare_snapshot(Path::new("extensionless"), b"none")
            .unwrap();

        assert_ne!(first.path(), second.path());
        for snapshot in [&first, &second, &unsafe_extension, &no_extension] {
            assert!(
                snapshot
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("remote-")
            );
        }
        assert_eq!(first.path().extension(), Some(OsStr::new("Component9")));
        assert_eq!(second.path().extension(), Some(OsStr::new("Component9")));
        assert_eq!(unsafe_extension.path().extension(), None);
        assert_eq!(no_extension.path().extension(), None);

        drop((first, second, unsafe_extension, no_extension));
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn traversal_unusual_and_unicode_names_remain_direct_children_of_the_private_root() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let root = store.root_path().unwrap();
        let sources = [
            "../../escape.rs",
            "/absolute/outside.md",
            r"..\..\windows\escape.toml",
            "nested/目录/💣.тxt",
            "..",
        ];

        let snapshots = sources
            .iter()
            .map(|source| {
                store
                    .prepare_snapshot(Path::new(source), source.as_bytes())
                    .unwrap()
            })
            .collect::<Vec<_>>();

        for snapshot in &snapshots {
            assert_eq!(snapshot.path().parent(), Some(root.as_path()));
            assert!(snapshot.path().starts_with(&root));
            assert!(snapshot.path().is_file());
        }
        assert!(!base.path().join("escape.rs").exists());
        assert!(!base.path().join("outside.md").exists());

        drop(snapshots);
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn snapshot_bytes_exactly_match_supplied_remote_bytes() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let remote = b"\0remote\r\nbytes\xff";

        let snapshot = store
            .prepare_snapshot(Path::new("binary.dat"), remote)
            .unwrap();

        assert_eq!(fs::read(snapshot.path()).unwrap(), remote);
        drop(snapshot);
        shutdown.shutdown().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn completed_snapshots_have_no_unix_write_bits() {
        use std::os::unix::fs::PermissionsExt;

        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("readonly.rs"), b"readonly")
            .unwrap();

        let mode = fs::metadata(snapshot.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0);

        drop(snapshot);
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn dropping_an_unretained_snapshot_deletes_its_file() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("ephemeral.rs"), b"ephemeral")
            .unwrap();
        let path = snapshot.path().to_path_buf();
        assert!(path.exists());

        drop(snapshot);

        assert!(!path.exists());
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn successful_launch_retains_snapshot_while_store_is_open() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("retained.rs"), b"retained")
            .unwrap();
        let snapshot_path = snapshot.path().to_path_buf();
        let (local, _tracker, guard) = local_and_guard(base.path());
        let mut launcher = RecordingLauncher::default();

        store
            .launch_and_retain(&local, snapshot, guard, &mut launcher)
            .unwrap();

        assert_eq!(launcher.calls, vec![(local.clone(), snapshot_path.clone())]);
        assert!(snapshot_path.exists());

        shutdown.shutdown().unwrap();
        assert!(!snapshot_path.exists());
    }

    #[test]
    fn launcher_failure_does_not_retain_the_snapshot() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("failed.rs"), b"failed")
            .unwrap();
        let snapshot_path = snapshot.path().to_path_buf();
        let (local, _tracker, guard) = local_and_guard(base.path());
        let mut launcher = RecordingLauncher::failing();

        let error = store
            .launch_and_retain(&local, snapshot, guard, &mut launcher)
            .unwrap_err();

        assert!(!snapshot_path.exists());
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<LauncherFailure>().is_some())
        );
        assert!(format!("{error:#}").contains("launching Zed native diff"));
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn startup_removes_a_recognizable_unlocked_stale_root() {
        let base = tempdir().unwrap();
        let stale = recognizable_root(base.path(), "stale123");
        fs::create_dir(stale.join("nested")).unwrap();
        fs::write(stale.join("nested").join("snapshot"), b"old").unwrap();

        let (store, shutdown) = store_in(&base);

        assert!(!stale.exists());
        assert!(store.root_path().unwrap().exists());
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn startup_preserves_a_recognizable_root_with_an_exclusively_held_lock() {
        let base = tempdir().unwrap();
        let stale = recognizable_root(base.path(), "active123");
        let held_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(stale.join(".lock"))
            .unwrap();
        FileExt::lock_exclusive(&held_lock).unwrap();

        let (_store, shutdown) = store_in(&base);

        assert!(stale.exists());
        shutdown.shutdown().unwrap();
        FileExt::unlock(&held_lock).unwrap();
    }

    #[test]
    fn startup_ignores_matching_non_directories_and_never_affects_siblings_or_base() {
        let base = tempdir().unwrap();
        let matching_file = base.path().join(format!("{ROOT_PREFIX}plainfile"));
        fs::write(&matching_file, b"not a root").unwrap();
        let sibling = base.path().join("unrelated-sibling");
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("keep"), b"keep").unwrap();
        let missing_lock = base.path().join(format!("{ROOT_PREFIX}missinglock"));
        fs::create_dir(&missing_lock).unwrap();
        let base_marker = base.path().join("base-marker");
        fs::write(&base_marker, b"base").unwrap();

        let (_store, shutdown) = store_in(&base);

        assert_eq!(fs::read(&matching_file).unwrap(), b"not a root");
        assert_eq!(fs::read(sibling.join("keep")).unwrap(), b"keep");
        assert!(missing_lock.exists());
        assert_eq!(fs::read(&base_marker).unwrap(), b"base");
        assert!(base.path().exists());
        shutdown.shutdown().unwrap();
        assert!(base.path().exists());
    }

    #[cfg(unix)]
    #[test]
    fn startup_ignores_matching_directory_symlinks_and_unsafe_lock_symlinks() {
        use std::os::unix::fs::symlink;

        let base = tempdir().unwrap();
        let target = base.path().join("symlink-target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"target").unwrap();
        let matching_symlink = base.path().join(format!("{ROOT_PREFIX}symlinked"));
        symlink(&target, &matching_symlink).unwrap();

        let unsafe_lock_root = base.path().join(format!("{ROOT_PREFIX}unsafelock"));
        fs::create_dir(&unsafe_lock_root).unwrap();
        let external_lock = base.path().join("external-lock");
        fs::write(&external_lock, b"external").unwrap();
        symlink(&external_lock, unsafe_lock_root.join(".lock")).unwrap();

        let (_store, shutdown) = store_in(&base);

        assert!(
            fs::symlink_metadata(&matching_symlink)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target.join("keep")).unwrap(), b"target");
        assert!(unsafe_lock_root.exists());
        assert_eq!(fs::read(&external_lock).unwrap(), b"external");
        assert!(base.path().exists());
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn explicit_shutdown_closes_store_and_removes_root_despite_worker_clones() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let worker = store.clone();
        let root = store.root_path().unwrap();
        let snapshot = store
            .prepare_snapshot(Path::new("retained.rs"), b"retained")
            .unwrap();
        let snapshot_path = snapshot.path().to_path_buf();
        let (local, _tracker, guard) = local_and_guard(base.path());
        store
            .launch_and_retain(&local, snapshot, guard, &mut RecordingLauncher::default())
            .unwrap();

        shutdown.shutdown().unwrap();

        assert!(shutdown.is_closed().unwrap());
        assert!(!snapshot_path.exists());
        assert!(!root.exists());
        assert!(
            worker
                .prepare_snapshot(Path::new("late.rs"), b"late")
                .is_err()
        );
    }

    #[test]
    fn shutdown_is_idempotent() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let root = store.root_path().unwrap();

        shutdown.shutdown().unwrap();
        shutdown.shutdown().unwrap();

        assert!(shutdown.is_closed().unwrap());
        assert!(!root.exists());
    }

    #[test]
    fn closed_store_refuses_creation_and_launch_and_drops_prepared_snapshot() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let prepared = store
            .prepare_snapshot(Path::new("prepared.rs"), b"prepared")
            .unwrap();
        let prepared_path = prepared.path().to_path_buf();
        let (local, _tracker, guard) = local_and_guard(base.path());
        shutdown.shutdown().unwrap();
        let mut launcher = RecordingLauncher::default();

        let launch_error = store
            .launch_and_retain(&local, prepared, guard, &mut launcher)
            .unwrap_err();

        assert!(format!("{launch_error:#}").contains("closed"));
        assert!(launcher.calls.is_empty());
        assert!(!prepared_path.exists());
        assert!(
            store
                .prepare_snapshot(Path::new("late.rs"), b"late")
                .is_err()
        );
    }

    #[test]
    fn launch_refuses_a_cancelled_operation_without_calling_launcher() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("cancelled.rs"), b"cancelled")
            .unwrap();
        let snapshot_path = snapshot.path().to_path_buf();
        let (local, mut tracker, guard) = local_and_guard(base.path());
        tracker.change(&local);
        let mut launcher = RecordingLauncher::default();

        let error = store
            .launch_and_retain(&local, snapshot, guard, &mut launcher)
            .unwrap_err();

        assert!(format!("{error:#}").contains("cancelled"));
        assert!(launcher.calls.is_empty());
        assert!(!snapshot_path.exists());
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn guard_is_claimed_once_immediately_before_launcher_is_called() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("claimed.rs"), b"claimed")
            .unwrap();
        let (local, _tracker, guard) = local_and_guard(base.path());
        let probe = guard.clone();
        let mut launcher = RecordingLauncher::probing(probe);

        store
            .launch_and_retain(&local, snapshot, guard.clone(), &mut launcher)
            .unwrap();

        assert!(launcher.observed_claim_before_launch);
        assert!(!guard.try_claim());
        assert_eq!(launcher.calls.len(), 1);
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn zed_command_is_constructed_with_exact_program_and_argument_order() {
        let local = Path::new("/workspace/project/local.rs");
        let remote = Path::new("/private/tmp/remote-snapshot.rs");

        let command = zed_diff_command(local, remote);
        let arguments = command.get_args().map(OsString::from).collect::<Vec<_>>();

        assert_eq!(command.get_program(), OsStr::new("zed"));
        assert_eq!(
            arguments,
            vec![
                OsString::from("--diff"),
                local.as_os_str().to_os_string(),
                remote.as_os_str().to_os_string(),
            ]
        );
    }

    #[test]
    fn fake_launcher_receives_exact_local_and_snapshot_paths() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("exact.rs"), b"exact")
            .unwrap();
        let remote = snapshot.path().to_path_buf();
        let (local, _tracker, guard) = local_and_guard(base.path());
        let mut launcher = RecordingLauncher::default();

        store
            .launch_and_retain(&local, snapshot, guard, &mut launcher)
            .unwrap();

        assert_eq!(launcher.calls, vec![(local, remote)]);
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn launcher_error_context_preserves_the_original_error() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("error.rs"), b"error")
            .unwrap();
        let (local, _tracker, guard) = local_and_guard(base.path());
        let mut launcher = RecordingLauncher::failing();

        let error = store
            .launch_and_retain(&local, snapshot, guard, &mut launcher)
            .unwrap_err();

        assert!(format!("{error:#}").contains("launching Zed native diff"));
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<LauncherFailure>().is_some())
        );
        shutdown.shutdown().unwrap();
    }

    #[test]
    fn recognizable_name_requires_an_ascii_alphanumeric_suffix() {
        assert!(super::is_recognizable_root_name(OsStr::new(
            "ferry-lsp-diff-v1-Abc123"
        )));
        assert!(!super::is_recognizable_root_name(OsStr::new(
            "ferry-lsp-diff-v1-"
        )));
        assert!(!super::is_recognizable_root_name(OsStr::new(
            "ferry-lsp-diff-v1-../other"
        )));
        assert!(!super::is_recognizable_root_name(OsStr::new(
            "ferry-lsp-diff-v1-unicode💣"
        )));
    }

    #[test]
    fn locked_root_test_uses_a_regular_lock_file() {
        let base = tempdir().unwrap();
        let root = recognizable_root(base.path(), "regularlock");
        let lock = File::open(root.join(".lock")).unwrap();
        assert!(lock.metadata().unwrap().is_file());
    }
}
