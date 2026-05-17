#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[must_use]
pub enum FileState {
    InSync,
    LocalChanged,
    RemoteChanged,
    BothChanged,   // conflict
    LocalOnly,
    RemoteOnly,
    Untracked,     // exists both, no known hash
}

/// Pure classification. Inputs are hashes; `None` means file does not exist (local/remote)
/// or has never been synced (known).
///
/// Caller contract: never invoke with `local == None && remote == None`. A path that
/// exists in neither place should be pruned from the union walk, not classified.
#[must_use]
pub fn classify(local: Option<&str>, remote: Option<&str>, known: Option<&str>) -> FileState {
    match (local, remote, known) {
        (None, None, _) => unreachable!("called with no file present"),
        (Some(_), None, _) => FileState::LocalOnly,
        (None, Some(_), _) => FileState::RemoteOnly,
        (Some(_), Some(_), None) => FileState::Untracked,
        (Some(l), Some(r), Some(k)) => match (l == k, r == k, l == r) {
            (true,  true,  _    ) => FileState::InSync,
            (false, true,  _    ) => FileState::LocalChanged,
            (true,  false, _    ) => FileState::RemoteChanged,
            (false, false, true ) => FileState::InSync,     // both moved, same target
            (false, false, false) => FileState::BothChanged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn in_sync()          { assert_eq!(classify(Some("a"), Some("a"), Some("a")), FileState::InSync); }
    #[test] fn local_changed()    { assert_eq!(classify(Some("b"), Some("a"), Some("a")), FileState::LocalChanged); }
    #[test] fn remote_changed()   { assert_eq!(classify(Some("a"), Some("b"), Some("a")), FileState::RemoteChanged); }
    #[test] fn both_changed()     { assert_eq!(classify(Some("b"), Some("c"), Some("a")), FileState::BothChanged); }
    #[test] fn both_changed_same() { assert_eq!(classify(Some("b"), Some("b"), Some("a")), FileState::InSync); }
    #[test] fn local_only()       { assert_eq!(classify(Some("a"), None, None), FileState::LocalOnly); }
    #[test] fn remote_only()      { assert_eq!(classify(None, Some("a"), None), FileState::RemoteOnly); }
    #[test] fn untracked()        { assert_eq!(classify(Some("a"), Some("a"), None), FileState::Untracked); }
    #[test] fn untracked_differ() { assert_eq!(classify(Some("a"), Some("b"), None), FileState::Untracked); }
}

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct StateFile {
    pub version: u32,
    #[serde(default)]
    pub files: BTreeMap<String, FileRecord>,
    #[serde(default)]
    pub server_supports_mdtm: Option<bool>,
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
        if !path.exists() {
            return Ok(Self { version: 1, ..Default::default() });
        }
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }
}

#[cfg(test)]
mod state_file_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = StateFile { version: 1, ..Default::default() };
        s.files.insert("src/x.html".into(), FileRecord {
            sha256: "abc".into(),
            size: 42,
            remote_mtime: Utc.with_ymd_and_hms(2026, 5, 17, 8, 0, 0).unwrap(),
            last_synced: Utc.with_ymd_and_hms(2026, 5, 17, 8, 1, 0).unwrap(),
        });
        s.save(&path).unwrap();
        let loaded = StateFile::load_or_default(&path).unwrap();
        assert_eq!(s, loaded);
    }

    #[test]
    fn missing_file_returns_default() {
        let s = StateFile::load_or_default(Path::new("/nonexistent/zedftp/state.json")).unwrap();
        assert_eq!(s.version, 1);
        assert!(s.files.is_empty());
    }
}
