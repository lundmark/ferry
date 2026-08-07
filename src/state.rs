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
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct StateFile {
    pub version: u32,
    #[serde(default)]
    pub files: BTreeMap<String, FileRecord>,
    #[serde(default)]
    pub server_supports_mdtm: Option<bool>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            files: BTreeMap::new(),
            server_supports_mdtm: None,
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

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating state dir {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)
            .with_context(|| format!("writing state file temp {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming state file into place at {}", path.display()))?;
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
