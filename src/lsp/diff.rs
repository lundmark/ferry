#![allow(dead_code)] // The protocol loop wires this runtime in the next change.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, anyhow, ensure};
use fs2::FileExt;
use same_file::Handle;
use tempfile::{Builder, NamedTempFile};

use super::document_state::OperationGuard;

const ROOT_PREFIX: &str = "ferry-lsp-diff-v1-";
const QUARANTINE_PREFIX: &str = "ferry-lsp-quarantine-v1-";
const QUARANTINED_ROOT_NAME: &str = "root";
const SNAPSHOT_PREFIX: &str = "remote-";
const LOCK_NAME: &str = ".lock";
const CHILD_REAPER_THREAD_NAME: &str = "ferry-zed-diff-reaper";
const MAX_EXTENSION_BYTES: usize = 32;

pub(crate) trait DiffLauncher: Send {
    fn launch(&mut self, local: &Path, remote: &Path) -> Result<()>;
}

pub(crate) struct ZedDiffLauncher;

struct ChildReaper {
    sender: Sender<Child>,
    waiter: JoinHandle<io::Result<ExitStatus>>,
}

impl ChildReaper {
    fn start() -> Result<Self> {
        let (sender, receiver) = channel::<Child>();
        let waiter = thread::Builder::new()
            .name(CHILD_REAPER_THREAD_NAME.to_owned())
            .spawn(move || {
                let mut child = receiver.recv().map_err(|_| {
                    io::Error::new(
                        ErrorKind::BrokenPipe,
                        "Zed child reaper closed before receiving a child",
                    )
                })?;
                child.wait()
            })
            .context("starting Zed child reaper")?;
        Ok(Self { sender, waiter })
    }

    fn accept(self, child: Child) -> JoinHandle<io::Result<ExitStatus>> {
        // The fresh worker cannot disconnect before this rendezvous: its only
        // operation before receipt is the blocking receive above.
        self.sender
            .send(child)
            .expect("fresh Zed child reaper must accept its child");
        self.waiter
    }
}

impl DiffLauncher for ZedDiffLauncher {
    fn launch(&mut self, local: &Path, remote: &Path) -> Result<()> {
        // Start the waiter before the child so thread-creation failure never
        // leaves an already-spawned process without a reaping owner.
        let reaper = ChildReaper::start()?;
        let child = zed_diff_command(local, remote)
            .spawn()
            .context("launching Zed native diff")?;
        drop(reaper.accept(child));
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
    root: Option<OwnedRoot>,
}

struct OwnedRoot {
    base: PathBuf,
    location: RootLocation,
    root_identity: Option<Handle>,
    lock_file: Option<File>,
    lock_identity: Option<Handle>,
    quarantine: Option<Quarantine>,
    #[cfg(test)]
    fail_after_move: bool,
    #[cfg(test)]
    fail_next_delete: bool,
}

enum RootLocation {
    Active(PathBuf),
    QuarantinedUnverified(PathBuf),
    QuarantinedVerified(PathBuf),
    Complete,
}

struct Quarantine {
    path: PathBuf,
    identity: Option<Handle>,
}

pub(crate) struct PreparedSnapshot {
    file: NamedTempFile,
}

impl PreparedSnapshot {
    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }
}

impl Drop for PreparedSnapshot {
    fn drop(&mut self) {
        #[cfg(not(unix))]
        clear_file_read_only(self.file.as_file());
    }
}

impl OwnedRoot {
    fn capture(base: PathBuf, source: PathBuf, lock_file: File) -> Result<Self> {
        let root_identity =
            Handle::from_path(&source).context("capturing private snapshot root identity")?;
        Self::capture_with_root_identity(base, source, lock_file, root_identity)
    }

    fn capture_with_root_identity(
        base: PathBuf,
        source: PathBuf,
        lock_file: File,
        root_identity: Handle,
    ) -> Result<Self> {
        ensure!(
            source != base && source.parent() == Some(base.as_path()),
            "private snapshot root is outside its canonical base"
        );
        validate_active_root(&source)?;
        ensure!(
            fs::canonicalize(&source).context("resolving owned private snapshot root")? == source,
            "owned private snapshot root does not resolve to itself"
        );

        let current_root =
            Handle::from_path(&source).context("rechecking private snapshot root identity")?;
        ensure!(
            current_root == root_identity,
            "private snapshot root identity changed during capture"
        );

        let lock_path = source.join(LOCK_NAME);
        validate_lock_path(&source, &lock_path, &lock_file)?;
        let lock_identity = Handle::from_file(
            lock_file
                .try_clone()
                .context("cloning private snapshot lock for identity")?,
        )
        .context("capturing private snapshot lock identity")?;
        let current_lock =
            Handle::from_path(&lock_path).context("rechecking private snapshot lock identity")?;
        ensure!(
            current_lock == lock_identity,
            "private snapshot lock identity changed during capture"
        );

        Ok(Self {
            base,
            location: RootLocation::Active(source),
            root_identity: Some(root_identity),
            lock_file: Some(lock_file),
            lock_identity: Some(lock_identity),
            quarantine: None,
            #[cfg(test)]
            fail_after_move: false,
            #[cfg(test)]
            fail_next_delete: false,
        })
    }

    fn active_path(&self) -> Option<&Path> {
        match &self.location {
            RootLocation::Active(path) => Some(path),
            _ => None,
        }
    }

    fn cleanup_path(&self) -> Option<&Path> {
        match &self.location {
            RootLocation::Active(path)
            | RootLocation::QuarantinedUnverified(path)
            | RootLocation::QuarantinedVerified(path) => Some(path),
            RootLocation::Complete => None,
        }
    }

    fn cleanup(&mut self) -> Result<()> {
        if matches!(self.location, RootLocation::Active(_)) {
            self.move_to_quarantine()?;
        }
        #[cfg(test)]
        if self.fail_after_move {
            self.fail_after_move = false;
            return Err(anyhow!("injected failure after quarantine move"));
        }
        if matches!(self.location, RootLocation::QuarantinedUnverified(_)) {
            self.verify_quarantined_root()?;
        }
        if matches!(self.location, RootLocation::QuarantinedVerified(_)) {
            self.delete_verified_quarantine()?;
        }
        Ok(())
    }

    fn move_to_quarantine(&mut self) -> Result<()> {
        let source = match &self.location {
            RootLocation::Active(path) => path.clone(),
            _ => return Ok(()),
        };
        self.ensure_quarantine()?;
        self.verify_quarantine_identity()?;

        let quarantine = self
            .quarantine
            .as_ref()
            .context("private snapshot quarantine is unavailable")?;
        let moved_root = quarantine.path.join(QUARANTINED_ROOT_NAME);
        match fs::symlink_metadata(&moved_root) {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(anyhow!(
                    "private snapshot quarantine destination already exists"
                ));
            }
            Err(error) => {
                return Err(error).context("inspecting private snapshot quarantine destination");
            }
        }

        fs::rename(&source, &moved_root).with_context(|| {
            format!(
                "moving private snapshot root {} into quarantine {}",
                source.display(),
                quarantine.path.display()
            )
        })?;
        // Rename is atomic. Record the moved path before any other fallible work.
        self.location = RootLocation::QuarantinedUnverified(moved_root);
        Ok(())
    }

    fn ensure_quarantine(&mut self) -> Result<()> {
        if self.quarantine.is_none() {
            let mut builder = Builder::new();
            builder.prefix(QUARANTINE_PREFIX).disable_cleanup(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(fs::Permissions::from_mode(0o700));
            }

            let quarantine = builder
                .tempdir_in(&self.base)
                .context("creating private snapshot quarantine")?;
            let identity = Handle::from_path(quarantine.path())
                .context("capturing private snapshot quarantine identity");
            let path = quarantine.keep();
            // Persist the path immediately: no TempDir drop may remove an
            // unverified quarantine after this point.
            self.quarantine = Some(Quarantine {
                path,
                identity: None,
            });
            self.quarantine
                .as_mut()
                .expect("quarantine was just stored")
                .identity = Some(identity?);
        }

        let quarantine = self
            .quarantine
            .as_ref()
            .context("private snapshot quarantine is unavailable")?;
        let identity = quarantine
            .identity
            .as_ref()
            .context("private snapshot quarantine identity is unavailable")?;
        set_private_directory_mode(identity.as_file())?;
        self.verify_quarantine_identity()
    }

    fn verify_quarantine_identity(&self) -> Result<()> {
        let quarantine = self
            .quarantine
            .as_ref()
            .context("private snapshot quarantine is unavailable")?;
        ensure!(
            quarantine.path.parent() == Some(self.base.as_path()),
            "private snapshot quarantine is outside its canonical base"
        );
        let metadata = fs::symlink_metadata(&quarantine.path)
            .context("inspecting private snapshot quarantine")?;
        ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "private snapshot quarantine is unsafe"
        );
        ensure!(
            fs::canonicalize(&quarantine.path).context("resolving private snapshot quarantine")?
                == quarantine.path,
            "private snapshot quarantine does not resolve to itself"
        );
        let expected = quarantine
            .identity
            .as_ref()
            .context("private snapshot quarantine identity is unavailable")?;
        let current = Handle::from_path(&quarantine.path)
            .context("rechecking private snapshot quarantine identity")?;
        ensure!(
            &current == expected,
            "private snapshot quarantine identity changed"
        );
        Ok(())
    }

    fn verify_quarantined_root(&mut self) -> Result<()> {
        let moved_root = match &self.location {
            RootLocation::QuarantinedUnverified(path) => path.clone(),
            _ => return Ok(()),
        };
        self.verify_quarantined_ownership(&moved_root)?;
        self.location = RootLocation::QuarantinedVerified(moved_root);
        Ok(())
    }

    fn verify_quarantined_ownership(&self, moved_root: &Path) -> Result<()> {
        self.verify_quarantine_identity()?;
        let quarantine_path = self
            .quarantine
            .as_ref()
            .context("private snapshot quarantine is unavailable")?
            .path
            .clone();
        ensure!(
            moved_root.parent() == Some(quarantine_path.as_path()),
            "quarantined private snapshot root is misplaced"
        );
        validate_active_root(moved_root)?;
        ensure!(
            fs::canonicalize(moved_root).context("resolving quarantined private snapshot root")?
                == moved_root,
            "quarantined private snapshot root does not resolve to itself"
        );

        let expected_root = self
            .root_identity
            .as_ref()
            .context("private snapshot root identity is unavailable")?;
        let current_root = Handle::from_path(moved_root)
            .context("rechecking moved private snapshot root identity")?;
        ensure!(
            &current_root == expected_root,
            "moved private snapshot root identity does not match its owner"
        );

        let moved_lock = moved_root.join(LOCK_NAME);
        let lock_file = self
            .lock_file
            .as_ref()
            .context("private snapshot lock is unavailable")?;
        validate_lock_path(moved_root, &moved_lock, lock_file)?;
        let expected_lock = self
            .lock_identity
            .as_ref()
            .context("private snapshot lock identity is unavailable")?;
        let current_lock = Handle::from_path(&moved_lock)
            .context("rechecking moved private snapshot lock identity")?;
        ensure!(
            &current_lock == expected_lock,
            "moved private snapshot lock identity does not match its owner"
        );
        Ok(())
    }

    fn delete_verified_quarantine(&mut self) -> Result<()> {
        let moved_root = match &self.location {
            RootLocation::QuarantinedVerified(path) => path.clone(),
            _ => return Err(anyhow!("private snapshot quarantine is not verified")),
        };
        self.verify_quarantined_ownership(&moved_root)?;
        let quarantine = self
            .quarantine
            .as_ref()
            .context("verified private snapshot quarantine is unavailable")?
            .path
            .clone();

        #[cfg(test)]
        if self.fail_next_delete {
            self.fail_next_delete = false;
            return Err(anyhow!("injected cleanup failure"));
        }

        prepare_tree_for_removal(&quarantine)?;
        fs::remove_dir_all(&quarantine).with_context(|| {
            format!(
                "removing verified private snapshot quarantine {}",
                quarantine.display()
            )
        })?;
        self.location = RootLocation::Complete;
        self.root_identity = None;
        self.lock_identity = None;
        self.lock_file = None;
        self.quarantine = None;
        Ok(())
    }

    #[cfg(test)]
    fn cleanup_path_for_test(&self) -> &Path {
        self.cleanup_path()
            .expect("test expected pending private snapshot cleanup")
    }

    #[cfg(test)]
    fn quarantine_path_for_test(&self) -> Option<&Path> {
        self.quarantine
            .as_ref()
            .map(|quarantine| quarantine.path.as_path())
    }

    #[cfg(test)]
    fn is_cleanup_complete_for_test(&self) -> bool {
        matches!(self.location, RootLocation::Complete)
    }

    #[cfg(test)]
    fn fail_after_move_for_test(&mut self) {
        self.fail_after_move = true;
    }

    #[cfg(test)]
    fn fail_next_delete_for_test(&mut self) {
        self.fail_next_delete = true;
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

        let mut root_builder = Builder::new();
        root_builder.prefix(ROOT_PREFIX).disable_cleanup(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            root_builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let root = root_builder
            .tempdir_in(&base)
            .context("creating private snapshot root")?;
        let root_identity = Handle::from_path(root.path())
            .context("capturing new private snapshot root identity")?;
        let root_path = root.keep();
        set_private_directory_mode(root_identity.as_file())?;

        let lock_path = root_path.join(LOCK_NAME);
        let mut lock_options = OpenOptions::new();
        lock_options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.mode(0o600);
        }
        let lock_file = lock_options
            .open(&lock_path)
            .context("creating private snapshot root lock")?;
        set_private_file_mode(&lock_file)?;
        FileExt::lock_exclusive(&lock_file).context("locking private snapshot root")?;
        let root =
            OwnedRoot::capture_with_root_identity(base, root_path, lock_file, root_identity)?;

        Ok(Self {
            inner: Arc::new(SnapshotInner {
                state: Mutex::new(SnapshotState {
                    closed: false,
                    retained: Vec::new(),
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
                .and_then(OwnedRoot::active_path)
                .map(Path::to_path_buf)
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
                .and_then(OwnedRoot::active_path)
                .context("private snapshot root is unavailable")?;
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
            .and_then(OwnedRoot::active_path)
            .context("private snapshot root is unavailable")?
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
            .and_then(OwnedRoot::active_path)
            .map(Path::to_path_buf)
            .context("private snapshot root is unavailable")
    }

    #[cfg(test)]
    fn fail_next_cleanup_for_test(&self) -> Result<()> {
        let mut state = self.inner.lock()?;
        let root = state
            .root
            .as_mut()
            .context("private snapshot root is unavailable")?;
        root.fail_next_delete_for_test();
        Ok(())
    }
}

impl SnapshotShutdown {
    pub(crate) fn shutdown(&self) -> Result<()> {
        let mut state = self.inner.lock()?;
        if !state.closed {
            state.closed = true;
            state.retained.clear();
        }

        let cleanup_complete = match state.root.as_mut() {
            Some(root) => {
                root.cleanup()?;
                matches!(root.location, RootLocation::Complete)
            }
            None => true,
        };
        if cleanup_complete {
            state.root = None;
        }
        Ok(())
    }

    #[cfg(test)]
    fn is_closed(&self) -> Result<bool> {
        Ok(self.inner.lock()?.closed)
    }

    #[cfg(test)]
    fn pending_cleanup_path_for_test(&self) -> Result<Option<PathBuf>> {
        Ok(self
            .inner
            .lock()?
            .root
            .as_ref()
            .and_then(OwnedRoot::cleanup_path)
            .map(Path::to_path_buf))
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
    let mut permissions = file
        .metadata()
        .context("inspecting private remote snapshot permissions")?
        .permissions();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = permissions.mode();
        permissions.set_mode(mode & !0o222);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);

    file.set_permissions(permissions)
        .context("making private remote snapshot read-only")
}

#[cfg(not(unix))]
fn clear_file_read_only(file: &File) {
    let Ok(metadata) = file.metadata() else {
        return;
    };
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        let _ = file.set_permissions(permissions);
    }
}

fn set_private_directory_mode(directory: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .context("setting private snapshot directory mode")?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn set_private_file_mode(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .context("setting private snapshot lock mode")?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn prepare_tree_for_removal(path: &Path) -> Result<()> {
    #[cfg(not(unix))]
    {
        clear_tree_read_only(path)?;
    }
    #[cfg(unix)]
    let _ = path;
    Ok(())
}

#[cfg(not(unix))]
fn clear_tree_read_only(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting cleanup path {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)
            .with_context(|| format!("reading cleanup directory {}", path.display()))?
        {
            let entry =
                entry.with_context(|| format!("reading cleanup entry below {}", path.display()))?;
            clear_tree_read_only(&entry.path())?;
        }
    }

    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("clearing read-only cleanup path {}", path.display()))?;
    }
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

fn validate_lock_path(root: &Path, lock_path: &Path, lock_file: &File) -> Result<()> {
    ensure!(
        lock_path.parent() == Some(root),
        "private snapshot lock is outside its root"
    );
    let metadata =
        fs::symlink_metadata(lock_path).context("inspecting private snapshot lock path")?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "private snapshot lock path is unsafe"
    );
    ensure!(
        fs::canonicalize(lock_path).context("resolving private snapshot lock path")? == lock_path,
        "private snapshot lock path does not resolve to itself"
    );
    ensure!(
        lock_file
            .metadata()
            .context("inspecting opened private snapshot lock")?
            .is_file(),
        "opened private snapshot lock is not a regular file"
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
    cleanup_stale_roots_with_hook(base, |_, _| {})
}

fn cleanup_stale_roots_with_hook(
    base: &Path,
    mut after_initial_identity: impl FnMut(&Path, &Path),
) -> Result<()> {
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
        let Ok(initial_root_identity) = Handle::from_path(&candidate) else {
            continue;
        };
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
        let Ok(current_root_identity) = Handle::from_path(&candidate) else {
            continue;
        };
        if current_root_identity != initial_root_identity {
            continue;
        }

        let lock_path = candidate.join(LOCK_NAME);
        let Ok(initial_lock_identity) = Handle::from_path(&lock_path) else {
            continue;
        };
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
        let Ok(current_lock_identity) = Handle::from_path(&lock_path) else {
            continue;
        };
        if current_lock_identity != initial_lock_identity {
            continue;
        }

        after_initial_identity(&candidate, &lock_path);

        let Ok(lock_file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
            continue;
        };
        let Ok(opened_lock_file) = lock_file.try_clone() else {
            continue;
        };
        let Ok(opened_lock_identity) = Handle::from_file(opened_lock_file) else {
            continue;
        };
        if opened_lock_identity != initial_lock_identity {
            continue;
        }
        let Ok(opened_metadata) = lock_file.metadata() else {
            continue;
        };
        if !opened_metadata.file_type().is_file() {
            continue;
        }
        match FileExt::try_lock_exclusive(&lock_file) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => continue,
            Err(_) => continue,
        }

        let Ok(mut root) = OwnedRoot::capture_with_root_identity(
            base.to_path_buf(),
            candidate.clone(),
            lock_file,
            initial_root_identity,
        ) else {
            continue;
        };
        root.cleanup().with_context(|| {
            format!(
                "cleaning stale private snapshot root {}",
                candidate.display()
            )
        })?;
    }
    Ok(())
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

    #[cfg(any(unix, windows))]
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

    #[cfg(any(unix, windows))]
    #[test]
    fn stale_scanner_rejects_replacement_after_initial_identity_capture() {
        let base = tempdir().unwrap();
        let canonical_base = fs::canonicalize(base.path()).unwrap();
        let candidate = recognizable_root(&canonical_base, "scannerreplace");
        let renamed_original = canonical_base.join("scanner-original");
        let mut swapped = false;

        super::cleanup_stale_roots_with_hook(
            &canonical_base,
            |observed_candidate, _observed_lock| {
                if observed_candidate != candidate {
                    return;
                }
                swapped = true;
                fs::rename(&candidate, &renamed_original).unwrap();
                fs::create_dir(&candidate).unwrap();
                fs::write(candidate.join(".lock"), b"replacement lock").unwrap();
                fs::write(candidate.join("replacement-sentinel"), b"must survive").unwrap();
            },
        )
        .unwrap();

        assert!(swapped);
        assert!(renamed_original.exists());
        assert_eq!(
            fs::read(candidate.join("replacement-sentinel")).unwrap(),
            b"must survive"
        );
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

    #[test]
    fn shutdown_rejects_a_replaced_root_path_without_deleting_either_directory() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let root = store.root_path().unwrap();
        let renamed_original = base.path().join("renamed-owned-root");
        fs::rename(&root, &renamed_original).unwrap();

        fs::create_dir(&root).unwrap();
        let replacement_sentinel = root.join("replacement-sentinel");
        fs::write(&replacement_sentinel, b"must survive").unwrap();

        let error = shutdown.shutdown().unwrap_err();
        let moved_replacement = shutdown.pending_cleanup_path_for_test().unwrap().unwrap();
        let quarantine = moved_replacement.parent().unwrap().to_path_buf();

        assert!(format!("{error:#}").contains("identity"));
        assert!(renamed_original.exists());
        assert!(!replacement_sentinel.exists());
        assert!(moved_replacement.join("replacement-sentinel").exists());
        assert!(shutdown.is_closed().unwrap());
        drop(store);
        drop(shutdown);
        assert!(renamed_original.exists());
        assert!(quarantine.exists());
        assert!(moved_replacement.join("replacement-sentinel").exists());
    }

    fn captured_root(base: &Path, candidate: &Path) -> super::OwnedRoot {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(candidate.join(".lock"))
            .unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        super::OwnedRoot::capture(base.to_path_buf(), candidate.to_path_buf(), lock).unwrap()
    }

    #[test]
    fn stale_cleanup_rejects_a_replaced_path_without_deleting_the_replacement() {
        let base = tempdir().unwrap();
        let canonical_base = fs::canonicalize(base.path()).unwrap();
        let candidate = recognizable_root(&canonical_base, "stalereplaced");
        let mut owned = captured_root(&canonical_base, &candidate);
        let renamed_original = canonical_base.join("renamed-stale-original");
        fs::rename(&candidate, &renamed_original).unwrap();

        let replacement = recognizable_root(&canonical_base, "stalereplaced");
        fs::write(replacement.join("replacement-sentinel"), b"must survive").unwrap();

        let error = owned.cleanup().unwrap_err();

        assert!(format!("{error:#}").contains("identity"));
        assert!(renamed_original.exists());
        assert!(
            owned
                .cleanup_path_for_test()
                .join("replacement-sentinel")
                .exists()
        );
    }

    #[test]
    fn successful_owned_root_cleanup_removes_the_verified_original() {
        let base = tempdir().unwrap();
        let canonical_base = fs::canonicalize(base.path()).unwrap();
        let candidate = recognizable_root(&canonical_base, "verifiedcleanup");
        let mut owned = captured_root(&canonical_base, &candidate);

        owned.cleanup().unwrap();

        assert!(!candidate.exists());
        assert!(owned.is_cleanup_complete_for_test());
        assert!(
            fs::read_dir(&canonical_base)
                .unwrap()
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(super::QUARANTINE_PREFIX))
        );
    }

    #[test]
    fn an_unverified_quarantine_is_not_deleted_when_its_owner_is_dropped() {
        let base = tempdir().unwrap();
        let canonical_base = fs::canonicalize(base.path()).unwrap();
        let candidate = recognizable_root(&canonical_base, "persistmismatch");
        let mut owned = captured_root(&canonical_base, &candidate);
        let renamed_original = canonical_base.join("persisted-original");
        fs::rename(&candidate, &renamed_original).unwrap();

        let replacement = recognizable_root(&canonical_base, "persistmismatch");
        fs::write(replacement.join("preserved-sentinel"), b"preserve").unwrap();
        owned.cleanup().unwrap_err();
        let quarantine = owned.quarantine_path_for_test().unwrap().to_path_buf();
        let moved_replacement = owned.cleanup_path_for_test().to_path_buf();

        drop(owned);

        assert!(quarantine.exists());
        assert_eq!(
            fs::read(moved_replacement.join("preserved-sentinel")).unwrap(),
            b"preserve"
        );
        assert!(renamed_original.exists());
    }

    #[test]
    fn shutdown_cleanup_failure_is_retryable_after_the_store_is_closed() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let root = store.root_path().unwrap();
        store.fail_next_cleanup_for_test().unwrap();

        let first_error = shutdown.shutdown().unwrap_err();
        let pending = shutdown.pending_cleanup_path_for_test().unwrap().unwrap();

        assert!(format!("{first_error:#}").contains("injected cleanup failure"));
        assert!(shutdown.is_closed().unwrap());
        assert!(pending.exists());
        assert!(!root.exists());
        assert!(
            store
                .prepare_snapshot(Path::new("closed.rs"), b"closed")
                .err()
                .unwrap()
                .to_string()
                .contains("closed")
        );

        shutdown.shutdown().unwrap();
        assert!(!pending.exists());
        assert!(shutdown.pending_cleanup_path_for_test().unwrap().is_none());
        shutdown.shutdown().unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn verified_quarantine_retry_rejects_a_replaced_path_without_deleting_it() {
        let base = tempdir().unwrap();
        let canonical_base = fs::canonicalize(base.path()).unwrap();
        let candidate = recognizable_root(&canonical_base, "retryreplace");
        let mut owned = captured_root(&canonical_base, &candidate);
        owned.fail_next_delete_for_test();

        let first_error = owned.cleanup().unwrap_err();
        let quarantine = owned.quarantine_path_for_test().unwrap().to_path_buf();
        let renamed_original = canonical_base.join("retry-original-quarantine");

        assert!(format!("{first_error:#}").contains("injected cleanup failure"));
        assert!(quarantine.exists());
        fs::rename(&quarantine, &renamed_original).unwrap();
        fs::create_dir(&quarantine).unwrap();
        fs::create_dir(quarantine.join("unrelated")).unwrap();
        fs::write(
            quarantine.join("unrelated").join("replacement-sentinel"),
            b"must survive",
        )
        .unwrap();

        let retry_error = owned.cleanup().unwrap_err();

        assert!(format!("{retry_error:#}").contains("identity"));
        assert_eq!(
            fs::read(quarantine.join("unrelated").join("replacement-sentinel")).unwrap(),
            b"must survive"
        );
        assert!(renamed_original.join(super::QUARANTINED_ROOT_NAME).exists());
        drop(owned);
        assert!(quarantine.exists());
        assert!(renamed_original.exists());
    }

    #[test]
    fn quarantine_move_holds_lock_and_is_not_recognized_by_another_scan() {
        let base = tempdir().unwrap();
        let canonical_base = fs::canonicalize(base.path()).unwrap();
        let candidate = recognizable_root(&canonical_base, "overlap");
        let mut owned = captured_root(&canonical_base, &candidate);
        owned.fail_after_move_for_test();

        owned.cleanup().unwrap_err();
        let quarantine = owned.quarantine_path_for_test().unwrap().to_path_buf();
        let moved_root = owned.cleanup_path_for_test().to_path_buf();

        assert!(quarantine.exists());
        assert!(moved_root.exists());
        assert!(!candidate.exists());
        assert!(
            !quarantine
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(ROOT_PREFIX)
        );
        let moved_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(moved_root.join(".lock"))
            .unwrap();
        assert!(matches!(
            FileExt::try_lock_exclusive(&moved_lock),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));

        super::cleanup_stale_roots(&canonical_base).unwrap();

        assert!(quarantine.exists());
        assert!(moved_root.exists());
        owned.cleanup().unwrap();
        assert!(!quarantine.exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_root_and_lock_have_exact_owner_only_modes() {
        use std::os::unix::fs::PermissionsExt;

        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let root = store.root_path().unwrap();

        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.join(".lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        shutdown.shutdown().unwrap();
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_snapshot_cleanup_clears_read_only_before_unlinking() {
        let base = tempdir().unwrap();
        let (store, shutdown) = store_in(&base);
        let snapshot = store
            .prepare_snapshot(Path::new("readonly.rs"), b"readonly")
            .unwrap();
        let path = snapshot.path().to_path_buf();

        assert!(fs::metadata(&path).unwrap().permissions().readonly());
        drop(snapshot);
        assert!(!path.exists());

        shutdown.shutdown().unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn child_reaper_waits_for_a_short_lived_child() {
        let reaper = super::ChildReaper::start().unwrap();
        #[cfg(unix)]
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        #[cfg(windows)]
        let child = std::process::Command::new("cmd")
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap();

        let waiter = reaper.accept(child);
        let status = waiter.join().unwrap().unwrap();

        assert!(status.success());
    }
}
