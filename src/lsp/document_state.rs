#![allow(dead_code)] // The protocol loop will use this state machine in a later change.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{Context, ensure};

const PENDING: u8 = 0;
const CANCELLED: u8 = 1;
const CLAIMED: u8 = 2;

#[derive(Clone)]
pub(crate) struct OperationGuard {
    state: Arc<AtomicU8>,
    revision: u64,
}

impl OperationGuard {
    pub(crate) fn try_claim(&self) -> bool {
        self.state
            .compare_exchange(PENDING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
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

#[derive(Default)]
pub(crate) struct DocumentTracker {
    entries: HashMap<PathBuf, DocumentEntry>,
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
    pub(crate) fn open(&mut self, path: PathBuf, text: &str) -> anyhow::Result<()> {
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
        if let Some(entry) = self.entries.get_mut(path) {
            entry.revision += 1;
            entry.dirty = true;
            entry.cancel_pending_guards();
        }
    }

    pub(crate) fn save(&mut self, path: &Path) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.revision += 1;
            entry.dirty = false;
            entry.cancel_pending_guards();
        }
    }

    pub(crate) fn close(&mut self, path: &Path) {
        if let Some(mut entry) = self.entries.remove(path) {
            entry.cancel_pending_guards();
        }
    }

    pub(crate) fn cancel_all(&mut self) {
        for entry in self.entries.values_mut() {
            entry.cancel_pending_guards();
        }
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
    use std::sync::atomic::Ordering;

    use tempfile::tempdir;

    use super::{CANCELLED, CLAIMED, DocumentStateError, DocumentTracker};

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
}
