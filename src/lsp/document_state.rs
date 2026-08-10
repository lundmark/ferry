#![allow(dead_code)] // The protocol loop will use this state machine in a later change.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{Context, ensure};

use crate::commands::sync::{CommitDecision, CommitGate};

const PENDING: u8 = 0;
const CANCELLED: u8 = 1;
const CLAIMED: u8 = 2;

const ACTIVE: u8 = 0;
const COMMITTING: u8 = 1;
const INVALIDATED: u8 = 2;
const INVALIDATED_DURING_COMMIT: u8 = COMMITTING | INVALIDATED;

#[derive(Clone)]
pub(crate) struct OperationGuard {
    state: Arc<AtomicU8>,
    revision: u64,
}

impl OperationGuard {
    pub(crate) fn is_pending(&self) -> bool {
        self.state.load(Ordering::Acquire) == PENDING
    }

    pub(crate) fn try_claim(&self) -> bool {
        self.state
            .compare_exchange(PENDING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn cancel(&self) {
        let _ =
            self.state
                .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocumentScope {
    Exact(PathBuf),
    Directory(PathBuf),
}

impl DocumentScope {
    fn contains(&self, path: &Path) -> bool {
        match self {
            Self::Exact(exact) => path == exact,
            Self::Directory(directory) => path.starts_with(directory),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ScopeOperationGuard {
    state: Arc<AtomicU8>,
}

struct ScopeCommitClaim {
    state: Arc<AtomicU8>,
}

impl Drop for ScopeCommitClaim {
    fn drop(&mut self) {
        match self
            .state
            .compare_exchange(COMMITTING, ACTIVE, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {}
            Err(INVALIDATED_DURING_COMMIT) => {
                self.state.store(INVALIDATED, Ordering::Release);
            }
            Err(current) => {
                debug_assert_eq!(current, INVALIDATED);
            }
        }
    }
}

impl CommitGate for ScopeOperationGuard {
    fn is_current(&self) -> bool {
        self.state.load(Ordering::Acquire) == ACTIVE
    }

    fn commit(
        &self,
        mutation: &mut dyn FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<CommitDecision> {
        if self
            .state
            .compare_exchange(ACTIVE, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(CommitDecision::Cancelled);
        }

        let claim = ScopeCommitClaim {
            state: Arc::clone(&self.state),
        };
        let result = mutation();
        drop(claim);
        result?;
        Ok(CommitDecision::Committed)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DocumentStateError {
    Dirty { path: PathBuf },
    Untracked { path: PathBuf },
}

impl fmt::Display for DocumentStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dirty { path } => {
                write!(
                    formatter,
                    "document has unsaved changes: {}",
                    path.display()
                )
            }
            Self::Untracked { path } => {
                write!(formatter, "document is not tracked: {}", path.display())
            }
        }
    }
}

impl Error for DocumentStateError {}

struct ScopeRegistration {
    scope: DocumentScope,
    state: Weak<AtomicU8>,
}

#[derive(Default)]
pub(crate) struct DocumentTracker {
    entries: HashMap<PathBuf, DocumentEntry>,
    scopes: Vec<ScopeRegistration>,
}

struct DocumentEntry {
    revision: u64,
    dirty: bool,
    guards: Vec<Weak<AtomicU8>>,
}

impl DocumentEntry {
    fn prune_guards(&mut self) {
        self.guards.retain(|guard| {
            guard
                .upgrade()
                .is_some_and(|state| state.load(Ordering::Acquire) == PENDING)
        });
    }

    fn cancel_pending_guards(&mut self) {
        for guard in self.guards.drain(..) {
            if let Some(state) = guard.upgrade() {
                let _ =
                    state.compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire);
            }
        }
    }
}

impl DocumentTracker {
    fn prune_scope_registrations(&mut self) {
        self.scopes.retain(|registration| {
            registration
                .state
                .upgrade()
                .is_some_and(|state| state.load(Ordering::Acquire) & INVALIDATED == 0)
        });
    }

    fn invalidate_matching_scopes(&mut self, path: &Path) {
        self.scopes.retain(|registration| {
            let Some(state) = registration.state.upgrade() else {
                return false;
            };
            if state.load(Ordering::Acquire) & INVALIDATED != 0 {
                return false;
            }
            if registration.scope.contains(path) {
                state.fetch_or(INVALIDATED, Ordering::AcqRel);
                false
            } else {
                true
            }
        });
    }

    fn invalidate_all_scopes(&mut self) {
        for registration in self.scopes.drain(..) {
            if let Some(state) = registration.state.upgrade() {
                state.fetch_or(INVALIDATED, Ordering::AcqRel);
            }
        }
    }

    pub(crate) fn open(&mut self, path: PathBuf, text: &str) -> anyhow::Result<()> {
        self.invalidate_matching_scopes(&path);
        let metadata = fs::metadata(&path)
            .with_context(|| format!("failed to inspect document {}", path.display()))?;
        ensure!(
            metadata.is_file(),
            "document is not a regular file: {}",
            path.display()
        );
        let disk_bytes = fs::read(&path)
            .with_context(|| format!("failed to read document {}", path.display()))?;
        let replacement = DocumentEntry {
            revision: 0,
            dirty: disk_bytes != text.as_bytes(),
            guards: Vec::new(),
        };

        if let Some(existing) = self.entries.get_mut(&path) {
            existing.cancel_pending_guards();
        }
        self.entries.insert(path, replacement);
        Ok(())
    }

    pub(crate) fn change(&mut self, path: &Path) {
        self.invalidate_matching_scopes(path);
        if let Some(entry) = self.entries.get_mut(path) {
            entry.revision += 1;
            entry.dirty = true;
            entry.cancel_pending_guards();
        }
    }

    pub(crate) fn save(&mut self, path: &Path) {
        self.invalidate_matching_scopes(path);
        if let Some(entry) = self.entries.get_mut(path) {
            entry.revision += 1;
            entry.dirty = false;
            entry.cancel_pending_guards();
        }
    }

    pub(crate) fn close(&mut self, path: &Path) {
        self.invalidate_matching_scopes(path);
        if let Some(mut entry) = self.entries.remove(path) {
            entry.cancel_pending_guards();
        }
    }

    pub(crate) fn cancel_all(&mut self) {
        for entry in self.entries.values_mut() {
            entry.cancel_pending_guards();
        }
        self.invalidate_all_scopes();
    }

    pub(crate) fn begin_clean_scope(
        &mut self,
        scope: DocumentScope,
    ) -> Result<ScopeOperationGuard, DocumentStateError> {
        self.prune_scope_registrations();
        if let Some(path) = self
            .entries
            .iter()
            .find_map(|(path, entry)| (entry.dirty && scope.contains(path)).then(|| path.clone()))
        {
            return Err(DocumentStateError::Dirty { path });
        }

        let state = Arc::new(AtomicU8::new(ACTIVE));
        self.scopes.push(ScopeRegistration {
            scope,
            state: Arc::downgrade(&state),
        });
        Ok(ScopeOperationGuard { state })
    }

    pub(crate) fn begin_clean_operation(
        &mut self,
        path: &Path,
    ) -> Result<OperationGuard, DocumentStateError> {
        let entry = self
            .entries
            .get_mut(path)
            .ok_or_else(|| DocumentStateError::Untracked {
                path: path.to_path_buf(),
            })?;
        entry.prune_guards();
        if entry.dirty {
            return Err(DocumentStateError::Dirty {
                path: path.to_path_buf(),
            });
        }

        let state = Arc::new(AtomicU8::new(PENDING));
        entry.guards.push(Arc::downgrade(&state));
        Ok(OperationGuard {
            state,
            revision: entry.revision,
        })
    }
}

impl Drop for DocumentTracker {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use crate::commands::sync::{CommitDecision, CommitGate};

    use super::{
        CANCELLED, CLAIMED, DocumentScope, DocumentStateError, DocumentTracker, ScopeOperationGuard,
    };

    fn write_file(directory: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn assert_dirty(result: Result<super::OperationGuard, DocumentStateError>, path: &Path) {
        assert!(matches!(
            result,
            Err(DocumentStateError::Dirty { path: error_path }) if error_path == path
        ));
    }

    fn assert_untracked(result: Result<super::OperationGuard, DocumentStateError>, path: &Path) {
        assert!(matches!(
            result,
            Err(DocumentStateError::Untracked { path: error_path }) if error_path == path
        ));
    }

    #[test]
    fn matching_open_text_is_clean_and_allows_an_operation() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "matching.txt", b"same bytes\n");
        let mut tracker = DocumentTracker::default();

        tracker.open(path.clone(), "same bytes\n").unwrap();
        let guard = tracker.begin_clean_operation(&path).unwrap();

        assert_eq!(guard.revision, 0);
        assert!(guard.try_claim());
    }

    #[test]
    fn differing_open_text_is_dirty_and_rejects_an_operation() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "dirty.txt", b"disk contents");
        let mut tracker = DocumentTracker::default();

        tracker.open(path.clone(), "editor contents").unwrap();

        assert_dirty(tracker.begin_clean_operation(&path), &path);
    }

    #[test]
    fn a_guard_and_its_clones_can_be_claimed_only_once_total() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "one-shot.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let guard = tracker.begin_clean_operation(&path).unwrap();
        let clone = guard.clone();

        assert!(clone.try_claim());
        assert!(!guard.try_claim());
        assert!(!clone.try_claim());
    }

    #[test]
    fn operation_guard_cancel_before_final_claim_denies_authorization() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "cancel-before-claim.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let guard = tracker.begin_clean_operation(&path).unwrap();

        guard.cancel();

        assert!(!guard.try_claim());
    }

    #[test]
    fn operation_guard_reports_pending_cancelled_and_claimed_states() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "pending-state.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let cancelled = tracker.begin_clean_operation(&path).unwrap();
        let claimed = tracker.begin_clean_operation(&path).unwrap();

        assert!(cancelled.is_pending());
        assert!(claimed.is_pending());

        cancelled.cancel();
        assert!(!cancelled.is_pending());

        assert!(claimed.try_claim());
        assert!(!claimed.is_pending());
    }

    #[test]
    fn operation_guard_final_claim_wins_before_late_cancellation() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "claim-before-cancel.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let guard = tracker.begin_clean_operation(&path).unwrap();

        assert!(guard.try_claim());
        guard.cancel();

        assert!(!guard.try_claim());
    }

    #[test]
    fn change_increments_revision_marks_dirty_and_cancels_all_pending_guards() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "changed.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let first = tracker.begin_clean_operation(&path).unwrap();
        let second = tracker.begin_clean_operation(&path).unwrap();

        tracker.change(&path);

        assert_eq!(tracker.entries.get(&path).unwrap().revision, 1);
        assert!(!first.try_claim());
        assert!(!second.try_claim());
        assert_dirty(tracker.begin_clean_operation(&path), &path);
    }

    #[test]
    fn save_advances_revision_invalidates_old_guards_and_allows_only_new_guards() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "saved.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let old_guard = tracker.begin_clean_operation(&path).unwrap();

        tracker.save(&path);
        let new_guard = tracker.begin_clean_operation(&path).unwrap();

        assert_eq!(new_guard.revision, 1);
        assert!(!old_guard.try_claim());
        assert!(new_guard.try_claim());
    }

    #[test]
    fn close_cancels_pending_guards_and_removes_tracking() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "closed.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let guard = tracker.begin_clean_operation(&path).unwrap();

        tracker.close(&path);

        assert!(!guard.try_claim());
        assert_untracked(tracker.begin_clean_operation(&path), &path);
    }

    #[test]
    fn lifecycle_events_for_one_file_do_not_cancel_another_files_guards() {
        let directory = tempdir().unwrap();
        let first_path = write_file(directory.path(), "first.txt", b"first");
        let second_path = write_file(directory.path(), "second.txt", b"second");
        let mut tracker = DocumentTracker::default();
        tracker.open(first_path.clone(), "first").unwrap();
        tracker.open(second_path.clone(), "second").unwrap();
        let first_guard = tracker.begin_clean_operation(&first_path).unwrap();
        let second_guard = tracker.begin_clean_operation(&second_path).unwrap();

        tracker.change(&first_path);

        assert!(!first_guard.try_claim());
        assert!(second_guard.try_claim());
    }

    #[test]
    fn cancel_all_cancels_pending_guards_across_documents_without_undoing_claims() {
        let directory = tempdir().unwrap();
        let first_path = write_file(directory.path(), "first.txt", b"first");
        let second_path = write_file(directory.path(), "second.txt", b"second");
        let mut tracker = DocumentTracker::default();
        tracker.open(first_path.clone(), "first").unwrap();
        tracker.open(second_path.clone(), "second").unwrap();
        let claimed = tracker.begin_clean_operation(&first_path).unwrap();
        let pending = tracker.begin_clean_operation(&second_path).unwrap();
        assert!(claimed.try_claim());

        tracker.cancel_all();

        assert_eq!(claimed.state.load(Ordering::Acquire), CLAIMED);
        assert_eq!(pending.state.load(Ordering::Acquire), CANCELLED);
        assert!(!pending.try_claim());
    }

    #[test]
    fn dropping_tracker_cancels_every_pending_guard() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "drop.txt", b"clean");
        let guard = {
            let mut tracker = DocumentTracker::default();
            tracker.open(path.clone(), "clean").unwrap();
            tracker.begin_clean_operation(&path).unwrap()
        };

        assert!(!guard.try_claim());
        assert_eq!(guard.state.load(Ordering::Acquire), CANCELLED);
    }

    #[test]
    fn claimed_and_dropped_guards_are_pruned_from_tracker_entries() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "pruned.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let claimed = tracker.begin_clean_operation(&path).unwrap();
        let dropped = tracker.begin_clean_operation(&path).unwrap();
        assert!(claimed.try_claim());
        drop(dropped);

        let pending = tracker.begin_clean_operation(&path).unwrap();

        assert_eq!(tracker.entries.get(&path).unwrap().guards.len(), 1);
        drop(pending);
        tracker.save(&path);
        assert!(tracker.entries.get(&path).unwrap().guards.is_empty());
    }

    #[test]
    fn missing_and_non_regular_open_targets_error_without_creating_tracking() {
        let directory = tempdir().unwrap();
        let missing = directory.path().join("missing.txt");
        let non_regular = directory.path().join("directory");
        fs::create_dir(&non_regular).unwrap();
        let mut tracker = DocumentTracker::default();

        assert!(tracker.open(missing.clone(), "").is_err());
        assert!(tracker.open(non_regular.clone(), "").is_err());

        assert_untracked(tracker.begin_clean_operation(&missing), &missing);
        assert_untracked(tracker.begin_clean_operation(&non_regular), &non_regular);
    }

    #[test]
    fn failed_reopen_preserves_the_existing_tracked_entry() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "preserved.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        fs::remove_file(&path).unwrap();

        assert!(tracker.open(path.clone(), "clean").is_err());

        assert!(tracker.begin_clean_operation(&path).unwrap().try_claim());
    }

    #[test]
    fn successful_reopen_cancels_existing_pending_guards_before_replacement() {
        let directory = tempdir().unwrap();
        let path = write_file(directory.path(), "reopened.txt", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let old_guard = tracker.begin_clean_operation(&path).unwrap();

        tracker.open(path.clone(), "different editor text").unwrap();

        assert!(!old_guard.try_claim());
        assert_dirty(tracker.begin_clean_operation(&path), &path);
    }

    fn assert_scope_dirty(result: Result<ScopeOperationGuard, DocumentStateError>, path: &Path) {
        assert!(matches!(
            result,
            Err(DocumentStateError::Dirty { path: error_path }) if error_path == path
        ));
    }

    #[test]
    fn scope_exact_and_directory_reject_dirty_documents_on_segment_boundaries() {
        let root = tempdir().unwrap();
        let area = root.path().join("area");
        let area_old = root.path().join("area-old");
        fs::create_dir(&area).unwrap();
        fs::create_dir(&area_old).unwrap();
        let exact = write_file(&area, "exact.c", b"disk");
        let nested = write_file(&area, "nested.c", b"disk");
        let near = write_file(&area_old, "near.c", b"disk");
        let mut tracker = DocumentTracker::default();
        tracker.open(exact.clone(), "editor").unwrap();

        assert_scope_dirty(
            tracker.begin_clean_scope(DocumentScope::Exact(exact.clone())),
            &exact,
        );
        tracker.save(&exact);
        tracker.open(nested.clone(), "editor").unwrap();
        assert_scope_dirty(
            tracker.begin_clean_scope(DocumentScope::Directory(area.clone())),
            &nested,
        );

        tracker.save(&nested);
        tracker.open(near.clone(), "editor").unwrap();
        let guard = tracker
            .begin_clean_scope(DocumentScope::Directory(area))
            .unwrap();
        assert!(guard.is_current());
    }

    #[test]
    fn scope_matching_lifecycle_events_invalidate_but_outside_events_do_not() {
        let root = tempdir().unwrap();
        let area = root.path().join("area");
        let outside = root.path().join("outside");
        fs::create_dir(&area).unwrap();
        fs::create_dir(&outside).unwrap();
        let inside = write_file(&area, "inside.c", b"inside");
        let outside_file = write_file(&outside, "outside.c", b"outside");

        for event in ["open", "change", "save", "close"] {
            let mut tracker = DocumentTracker::default();
            if event != "open" {
                tracker.open(inside.clone(), "inside").unwrap();
            }
            let guard = tracker
                .begin_clean_scope(DocumentScope::Directory(area.clone()))
                .unwrap();
            match event {
                "open" => tracker.open(inside.clone(), "inside").unwrap(),
                "change" => tracker.change(&inside),
                "save" => tracker.save(&inside),
                "close" => tracker.close(&inside),
                _ => unreachable!(),
            }
            assert!(!guard.is_current(), "{event} must invalidate the scope");
        }

        for event in ["open", "change", "save", "close"] {
            let mut tracker = DocumentTracker::default();
            if event != "open" {
                tracker.open(outside_file.clone(), "outside").unwrap();
            }
            let guard = tracker
                .begin_clean_scope(DocumentScope::Directory(area.clone()))
                .unwrap();
            match event {
                "open" => tracker.open(outside_file.clone(), "outside").unwrap(),
                "change" => tracker.change(&outside_file),
                "save" => tracker.save(&outside_file),
                "close" => tracker.close(&outside_file),
                _ => unreachable!(),
            }
            assert!(
                guard.is_current(),
                "outside {event} must not invalidate the scope"
            );
        }
    }

    #[test]
    fn scope_cancel_all_and_tracker_drop_invalidate_registrations() {
        let root = tempdir().unwrap();
        let area = root.path().join("area");
        fs::create_dir(&area).unwrap();
        let mut tracker = DocumentTracker::default();
        let cancelled = tracker
            .begin_clean_scope(DocumentScope::Directory(area.clone()))
            .unwrap();
        tracker.cancel_all();
        assert!(!cancelled.is_current());

        let dropped = {
            let mut tracker = DocumentTracker::default();
            tracker
                .begin_clean_scope(DocumentScope::Directory(area))
                .unwrap()
        };
        assert!(!dropped.is_current());
    }

    #[test]
    fn scope_dead_weak_registrations_are_pruned_before_the_next_registration() {
        let root = tempdir().unwrap();
        let mut tracker = DocumentTracker::default();
        let dropped = tracker
            .begin_clean_scope(DocumentScope::Directory(root.path().to_path_buf()))
            .unwrap();
        assert_eq!(tracker.scopes.len(), 1);
        drop(dropped);
        assert_eq!(
            tracker.scopes.len(),
            1,
            "registration stores only a dead weak ref"
        );

        let live = tracker
            .begin_clean_scope(DocumentScope::Directory(root.path().to_path_buf()))
            .unwrap();
        assert_eq!(tracker.scopes.len(), 1, "dead registration must be pruned");

        tracker.cancel_all();
        assert!(tracker.scopes.is_empty());
        assert!(!live.is_current());
    }

    #[test]
    fn commit_clean_claim_is_reusable_and_invokes_each_mutation_once() {
        let root = tempdir().unwrap();
        let mut tracker = DocumentTracker::default();
        let guard = tracker
            .begin_clean_scope(DocumentScope::Directory(root.path().to_path_buf()))
            .unwrap();
        let calls = AtomicUsize::new(0);

        for expected in 1..=2 {
            let decision = guard
                .commit(&mut || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            assert_eq!(decision, CommitDecision::Committed);
            assert_eq!(calls.load(Ordering::SeqCst), expected);
            assert!(guard.is_current());
        }
    }

    #[test]
    fn commit_invalidation_from_active_denies_mutation() {
        let root = tempdir().unwrap();
        let path = write_file(root.path(), "active.c", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let guard = tracker
            .begin_clean_scope(DocumentScope::Exact(path.clone()))
            .unwrap();
        tracker.change(&path);
        let mut called = false;

        let decision = guard
            .commit(&mut || {
                called = true;
                Ok(())
            })
            .unwrap();

        assert_eq!(decision, CommitDecision::Cancelled);
        assert!(!called);
    }

    #[test]
    fn commit_invalidation_while_claimed_is_nonblocking_and_permanent() {
        let root = tempdir().unwrap();
        let path = write_file(root.path(), "claimed.c", b"clean");
        let mut tracker = DocumentTracker::default();
        tracker.open(path.clone(), "clean").unwrap();
        let guard = tracker
            .begin_clean_scope(DocumentScope::Exact(path.clone()))
            .unwrap();
        let worker_guard = guard.clone();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);

        let worker = thread::spawn(move || {
            worker_guard.commit(&mut || {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release claimed mutation");
                Ok(())
            })
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("mutation claimed");

        tracker.save(&path);
        release_tx.send(()).unwrap();

        assert_eq!(worker.join().unwrap().unwrap(), CommitDecision::Committed);
        assert!(!guard.is_current());
        assert_eq!(
            guard
                .commit(&mut || panic!("late mutation must be denied"))
                .unwrap(),
            CommitDecision::Cancelled
        );
    }

    #[test]
    fn commit_concurrent_claims_are_exclusive_then_clean_release_reopens_gate() {
        let root = tempdir().unwrap();
        let mut tracker = DocumentTracker::default();
        let guard = tracker
            .begin_clean_scope(DocumentScope::Directory(root.path().to_path_buf()))
            .unwrap();
        let worker_guard = guard.clone();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);

        let worker = thread::spawn(move || {
            worker_guard.commit(&mut || {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("release first claim");
                Ok(())
            })
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first claim entered");

        assert_eq!(
            guard
                .commit(&mut || panic!("concurrent mutation must not run"))
                .unwrap(),
            CommitDecision::Cancelled
        );
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap(), CommitDecision::Committed);
        assert_eq!(
            guard.commit(&mut || Ok(())).unwrap(),
            CommitDecision::Committed
        );
    }
}
