#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[must_use]
pub enum FileState {
    InSync,
    LocalChanged,
    RemoteChanged,
    BothChanged, // conflict
    LocalOnly,
    RemoteOnly,
    Untracked, // exists both, no known hash
}

/// Pure classification. Inputs are hashes; `None` means file does not exist (local/remote)
/// or has never been synced (known).
///
/// Caller contract: never invoke with `local == None && remote == None`. A path that
/// exists in neither place should be pruned from the union walk, not classified.
pub fn classify(local: Option<&str>, remote: Option<&str>, known: Option<&str>) -> FileState {
    match (local, remote, known) {
        (None, None, _) => unreachable!("called with no file present"),
        (Some(_), None, _) => FileState::LocalOnly,
        (None, Some(_), _) => FileState::RemoteOnly,
        (Some(_), Some(_), None) => FileState::Untracked,
        (Some(l), Some(r), Some(k)) => match (l == k, r == k, l == r) {
            (true, true, _) => FileState::InSync,
            (false, true, _) => FileState::LocalChanged,
            (true, false, _) => FileState::RemoteChanged,
            (false, false, true) => FileState::InSync, // both moved, same target
            (false, false, false) => FileState::BothChanged,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_sync() {
        assert_eq!(classify(Some("a"), Some("a"), Some("a")), FileState::InSync);
    }
    #[test]
    fn local_changed() {
        assert_eq!(
            classify(Some("b"), Some("a"), Some("a")),
            FileState::LocalChanged
        );
    }
    #[test]
    fn remote_changed() {
        assert_eq!(
            classify(Some("a"), Some("b"), Some("a")),
            FileState::RemoteChanged
        );
    }
    #[test]
    fn both_changed() {
        assert_eq!(
            classify(Some("b"), Some("c"), Some("a")),
            FileState::BothChanged
        );
    }
    #[test]
    fn both_changed_same() {
        assert_eq!(classify(Some("b"), Some("b"), Some("a")), FileState::InSync);
    }
    #[test]
    fn local_only() {
        assert_eq!(classify(Some("a"), None, None), FileState::LocalOnly);
    }
    #[test]
    fn remote_only() {
        assert_eq!(classify(None, Some("a"), None), FileState::RemoteOnly);
    }
    #[test]
    fn untracked() {
        assert_eq!(classify(Some("a"), Some("a"), None), FileState::Untracked);
    }
    #[test]
    fn untracked_differ() {
        assert_eq!(classify(Some("a"), Some("b"), None), FileState::Untracked);
    }
}

use anyhow::Context;
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    #[serde(default)]
    pub files: BTreeMap<String, FileRecord>,
    #[serde(default)]
    pub server_supports_mdtm: Option<bool>,
    /// This process's view of `files` as of its load or last save. `save`
    /// writes only the entries that differ from it, merged into whatever is
    /// on disk at that moment, so concurrent ferry processes (a long pull,
    /// the editor hook, another session's push) no longer overwrite each
    /// other's records. `None` for a state that was never loaded: then every
    /// entry counts as this process's own.
    /// Public only so callers outside the crate (the integration tests) can
    /// still build a state with `..Default::default()`; not meant to be set.
    #[serde(skip)]
    #[doc(hidden)]
    pub baseline: RefCell<Option<BTreeMap<String, FileRecord>>>,
}

impl PartialEq for StateFile {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
            && self.files == other.files
            && self.server_supports_mdtm == other.server_supports_mdtm
    }
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            files: BTreeMap::new(),
            server_supports_mdtm: None,
            baseline: RefCell::new(None),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct FileRecord {
    pub sha256: String,
    pub size: u64,
    pub remote_mtime: DateTime<Utc>,
    pub last_synced: DateTime<Utc>,
}

impl StateFile {
    pub fn load_or_default(path: &Path) -> anyhow::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("reading state file {}", path.display()))
                );
            }
        };
        let parsed: Self = serde_json::from_str(&text)
            .with_context(|| format!("parsing state file {}", path.display()))?;
        parsed.baseline.replace(Some(parsed.files.clone()));
        if parsed.version != STATE_VERSION {
            anyhow::bail!(
                "state file {} has version {} but this binary only understands version {}",
                path.display(),
                parsed.version,
                STATE_VERSION,
            );
        }
        Ok(parsed)
    }

    /// Write this process's changes since its load (or last save) into the
    /// state file, under an exclusive lock, on top of what is on disk now.
    /// Entries other processes changed meanwhile are kept; for an entry both
    /// changed, this process's value wins.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state dir {}", parent.display()))?;
        }
        let lock_path = path.with_extension("json.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening state lock {}", lock_path.display()))?;
        FileExt::lock_exclusive(&lock)
            .with_context(|| format!("locking state file {}", path.display()))?;

        // An unreadable file is replaced, as the unmerged save always did.
        let mut merged = Self::load_or_default(path).unwrap_or_default();
        {
            let baseline = self.baseline.borrow();
            for (rel, record) in &self.files {
                let unchanged = baseline
                    .as_ref()
                    .is_some_and(|base| base.get(rel) == Some(record));
                if !unchanged {
                    merged.files.insert(rel.clone(), record.clone());
                }
            }
            if let Some(base) = baseline.as_ref() {
                for rel in base.keys() {
                    if !self.files.contains_key(rel) {
                        merged.files.remove(rel);
                    }
                }
            }
        }
        if self.server_supports_mdtm.is_some() {
            merged.server_supports_mdtm = self.server_supports_mdtm;
        }

        let text = serde_json::to_string_pretty(&merged)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)
            .with_context(|| format!("writing state file temp {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming state file into place at {}", path.display()))?;
        self.baseline.replace(Some(self.files.clone()));
        FileExt::unlock(&lock).ok();
        Ok(())
    }
}

#[cfg(test)]
mod state_file_tests {

    fn record(hash: &str) -> FileRecord {
        FileRecord {
            sha256: hash.into(),
            size: 1,
            remote_mtime: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            last_synced: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        }
    }

    #[test]
    fn concurrent_saves_keep_each_others_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut seed = StateFile::default();
        seed.files.insert("a".into(), record("a0"));
        seed.files.insert("b".into(), record("b0"));
        seed.files.insert("gone".into(), record("g0"));
        seed.save(&path).unwrap();

        // Two processes load the same file, then change different entries.
        let mut one = StateFile::load_or_default(&path).unwrap();
        let mut two = StateFile::load_or_default(&path).unwrap();
        one.files.insert("a".into(), record("a1"));
        one.files.remove("gone");
        two.files.insert("b".into(), record("b1"));
        two.files.insert("c".into(), record("c1"));
        one.save(&path).unwrap();
        two.save(&path).unwrap();

        let disk = StateFile::load_or_default(&path).unwrap();
        assert_eq!(disk.files["a"], record("a1"));
        assert_eq!(disk.files["b"], record("b1"));
        assert_eq!(disk.files["c"], record("c1"));
        assert!(!disk.files.contains_key("gone"));

        // A later save from `one` must not revert what `two` wrote.
        one.files.insert("d".into(), record("d1"));
        one.save(&path).unwrap();
        let disk = StateFile::load_or_default(&path).unwrap();
        assert_eq!(disk.files["b"], record("b1"));
        assert_eq!(disk.files["d"], record("d1"));
    }
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = StateFile {
            server_supports_mdtm: Some(true),
            ..StateFile::default()
        };
        s.files.insert(
            "src/x.html".into(),
            FileRecord {
                sha256: "abc".into(),
                size: 42,
                remote_mtime: Utc.with_ymd_and_hms(2026, 5, 17, 8, 0, 0).unwrap(),
                last_synced: Utc.with_ymd_and_hms(2026, 5, 17, 8, 1, 0).unwrap(),
            },
        );
        s.save(&path).unwrap();
        let loaded = StateFile::load_or_default(&path).unwrap();
        assert_eq!(s, loaded);
    }

    #[test]
    fn missing_file_returns_default() {
        let s = StateFile::load_or_default(Path::new("/nonexistent/zedftp/state.json")).unwrap();
        assert_eq!(s.version, STATE_VERSION);
        assert!(s.files.is_empty());
    }

    #[test]
    fn default_uses_current_version() {
        assert_eq!(StateFile::default().version, STATE_VERSION);
    }

    #[test]
    fn rejects_unknown_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"version": 99, "files": {}}"#).unwrap();
        let err = StateFile::load_or_default(&path).unwrap_err();
        assert!(err.to_string().contains("version 99"), "got: {err}");
    }
}
