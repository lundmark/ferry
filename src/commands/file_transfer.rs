use crate::ftp::{ExactFilePresence, Remote};
use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    Unchanged,
    Transferred,
    SkippedMissingSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub path: String,
    pub status: TransferStatus,
}

impl TransferOutcome {
    pub fn new(path: &str, status: TransferStatus) -> Self {
        Self {
            path: path.to_string(),
            status,
        }
    }
}

/// Proven remote-file presence. A failed metadata probe is never represented
/// as `Missing`: callers must return the indeterminate error instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePresence {
    Present,
    Missing,
}

/// Determine whether `path` names a remote file without failing open.
///
/// A successful `SIZE` proves presence. Servers that do not support `SIZE`
/// are common, so its error is followed by a listing of the parent directory.
/// A successful listing proves either presence or absence; when neither
/// operation succeeds we preserve both errors and make no transfer decision.
pub fn probe_remote_file<R: Remote + ?Sized>(remote: &mut R, path: &str) -> Result<RemotePresence> {
    match remote.file_size(path) {
        Ok(_) => Ok(RemotePresence::Present),
        Err(size_error) => {
            match remote.exact_file_presence(path) {
                Ok(ExactFilePresence::Present) => Ok(RemotePresence::Present),
                Ok(ExactFilePresence::Missing) => Ok(RemotePresence::Missing),
                Err(exact_error) => Err(size_error).with_context(|| {
                    format!(
                        "remote presence for {path} is indeterminate after exact lookup: {exact_error:#}"
                    )
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RemotePresence, TransferOutcome, TransferStatus, probe_remote_file};
    use crate::ftp::{Entry, ExactFilePresence, Remote};
    use anyhow::Result;
    use chrono::Utc;

    struct ScriptedRemote {
        size: Option<Result<u64>>,
        listing: Option<Result<Vec<Entry>>>,
        exact: Option<Result<ExactFilePresence>>,
    }

    impl Remote for ScriptedRemote {
        fn list_dir(&mut self, _dir: &str) -> Result<Vec<Entry>> {
            self.listing.take().expect("one LIST call")
        }

        fn file_size(&mut self, _path: &str) -> Result<u64> {
            self.size.take().expect("one SIZE call")
        }

        fn exact_file_presence(&mut self, _path: &str) -> Result<ExactFilePresence> {
            self.exact.take().expect("one exact probe call")
        }
    }

    fn file(name: &str, size: u64) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir: false,
            size,
            modified: Utc::now(),
        }
    }

    #[test]
    fn outcome_keeps_the_relative_path_and_status() {
        assert_eq!(
            TransferOutcome::new("src/main.rs", TransferStatus::Transferred),
            TransferOutcome {
                path: "src/main.rs".to_string(),
                status: TransferStatus::Transferred,
            }
        );
    }

    #[test]
    fn size_failure_falls_back_to_listing_an_existing_file() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Ok(vec![file("target.txt", 7)])),
            exact: Some(Ok(ExactFilePresence::Present)),
        };

        assert_eq!(
            probe_remote_file(&mut remote, "/home/test/target.txt").unwrap(),
            RemotePresence::Present
        );
    }

    #[test]
    fn size_failure_accepts_a_listing_that_echoes_the_full_file_path() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Ok(vec![file("/home/test/target.txt", 7)])),
            exact: Some(Ok(ExactFilePresence::Present)),
        };

        assert_eq!(
            probe_remote_file(&mut remote, "/home/test/target.txt").unwrap(),
            RemotePresence::Present
        );
    }

    #[test]
    fn size_failure_listing_proves_a_file_is_missing() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Ok(vec![])),
            exact: Some(Ok(ExactFilePresence::Missing)),
        };

        assert_eq!(
            probe_remote_file(&mut remote, "/home/test/target.txt").unwrap(),
            RemotePresence::Missing
        );
    }

    #[test]
    fn size_and_listing_failure_is_indeterminate() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Err(anyhow::anyhow!("LIST permission denied"))),
            exact: Some(Err(anyhow::anyhow!("NLST permission denied"))),
        };

        let error = probe_remote_file(&mut remote, "/home/test/target.txt").unwrap_err();

        assert!(format!("{error:#}").contains("SIZE unsupported"));
        assert!(format!("{error:#}").contains("NLST permission denied"));
    }

    #[test]
    fn incomplete_parent_listing_cannot_prove_exact_file_absence() {
        let mut remote = ScriptedRemote {
            size: Some(Err(anyhow::anyhow!("SIZE unsupported"))),
            listing: Some(Ok(vec![file("other.txt", 3)])),
            exact: Some(Err(anyhow::anyhow!("NLST malformed response"))),
        };

        let error = probe_remote_file(&mut remote, "/home/test/target.txt").unwrap_err();

        assert!(format!("{error:#}").contains("NLST malformed response"));
    }
}
