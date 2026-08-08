//! MDTM/SIZE fast-path for computing remote file hashes.
//!
//! Every command that calls `classify()` needs a remote hash. The naive path is
//! "download and SHA-256 the bytes" — which is fine for a small project but
//! expensive on every status check.
//!
//! Optimization: before downloading, compare the remote file's `MDTM` (mtime)
//! and `SIZE` against the cached values in `state.files[rel]`. If both match
//! we trust the cached `sha256` and skip the download entirely.
//!
//! Falls back to always-hashing when the server doesn't support MDTM. The
//! fallback decision is cached in `state.server_supports_mdtm` so we don't
//! re-probe every run.

use crate::ftp::Ftp;
use crate::hash::hash_bytes;
use crate::state::StateFile;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

pub(crate) trait RemoteFileRetrieval {
    fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>>;
    fn size(&mut self, remote_path: &str) -> Result<u64>;
    fn download(&mut self, remote_path: &str) -> Result<Vec<u8>>;
}

impl RemoteFileRetrieval for Ftp {
    fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        Ftp::mtime(self, remote_path)
    }

    fn size(&mut self, remote_path: &str) -> Result<u64> {
        Ftp::size(self, remote_path)
    }

    fn download(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        Ftp::download(self, remote_path)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RemoteMetadata {
    mtime: DateTime<Utc>,
    size: Option<u64>,
}

/// Result of computing a remote hash, either by trusting cached state or by
/// downloading and hashing.
#[derive(Debug)]
pub struct RemoteHash {
    pub sha256: String,
    pub size: u64,
    pub mtime: DateTime<Utc>,
    /// True if we skipped the download because (mtime, size) matched state.
    pub from_cache: bool,
    /// The downloaded bytes — populated only when we actually downloaded AND
    /// the caller asked for them via `want_bytes`. `pull`/`sync` need these
    /// for the local write; `status` doesn't.
    pub bytes: Option<Vec<u8>>,
    /// The cache probe that must be reused if classification later requires
    /// an install. `None` means the bytes were already retrieved, or MDTM was
    /// unavailable and the single retrieval could not be metadata-validated.
    pub(crate) pre_download: Option<RemoteMetadata>,
}

/// Compute the remote hash for `remote_path`, using the MDTM/SIZE fast path
/// when the server supports MDTM and the cached state agrees.
///
/// - `want_bytes`: if true, the caller wants the downloaded bytes in the
///   non-cached case (pull/sync need them for the local write). When the
///   fast path fires we still return `bytes: None` — and that's fine, because
///   `from_cache == true` means the file is `InSync` so no write is needed.
///   When the fast path can't fire (no cached entry, mismatch, MDTM
///   unsupported) we download anyway, and `want_bytes` decides whether we
///   hand the bytes back or drop them.
pub fn compute(
    ftp: &mut Ftp,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    want_bytes: bool,
) -> Result<RemoteHash> {
    compute_with(ftp, state, rel, remote_path, want_bytes)
}

pub(crate) fn compute_with<R: RemoteFileRetrieval>(
    remote: &mut R,
    state: &mut StateFile,
    rel: &str,
    remote_path: &str,
    want_bytes: bool,
) -> Result<RemoteHash> {
    let pre_download = if state.server_supports_mdtm.unwrap_or(true) {
        match remote.mtime(remote_path) {
            Ok(mtime) => {
                let size = remote.size(remote_path).ok();
                if state.server_supports_mdtm.is_none() {
                    state.server_supports_mdtm = Some(true);
                }
                Some(RemoteMetadata { mtime, size })
            }
            Err(_) => {
                state.server_supports_mdtm = Some(false);
                None
            }
        }
    } else {
        None
    };

    if let (Some(known), Some(observed)) = (state.files.get(rel), pre_download)
        && observed.size == Some(known.size)
        && observed.mtime == known.remote_mtime
    {
        return Ok(RemoteHash {
            sha256: known.sha256.clone(),
            size: known.size,
            mtime: observed.mtime,
            from_cache: true,
            bytes: None,
            pre_download: Some(observed),
        });
    }

    match pre_download {
        Some(observed) => download_after_observation(remote, remote_path, observed, want_bytes),
        None => download_unvalidated(remote, remote_path, want_bytes),
    }
}

pub(crate) fn complete_for_install<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
    remote_hash: RemoteHash,
) -> Result<RemoteHash> {
    if remote_hash.bytes.is_some() {
        return Ok(remote_hash);
    }
    let observed = remote_hash.pre_download.ok_or_else(|| {
        anyhow::anyhow!("remote hash for {remote_path} has no bytes or pre-download metadata")
    })?;
    download_after_observation(remote, remote_path, observed, true)
}

pub(crate) fn retrieve_stable<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
) -> Result<RemoteHash> {
    let observed = observe(remote, remote_path)?;
    download_after_observation(remote, remote_path, observed, true)
}

fn observe<R: RemoteFileRetrieval>(remote: &mut R, remote_path: &str) -> Result<RemoteMetadata> {
    let mtime = remote
        .mtime(remote_path)
        .with_context(|| format!("fetching mtime for {remote_path} before download"))?;
    let size = remote.size(remote_path).ok();
    Ok(RemoteMetadata { mtime, size })
}

fn download_after_observation<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
    before: RemoteMetadata,
    want_bytes: bool,
) -> Result<RemoteHash> {
    let bytes = remote
        .download(remote_path)
        .with_context(|| format!("downloading {remote_path}"))?;
    let after = RemoteMetadata {
        mtime: remote
            .mtime(remote_path)
            .with_context(|| format!("fetching mtime for {remote_path} after download"))?,
        size: remote.size(remote_path).ok(),
    };
    let size_changed = matches!((before.size, after.size), (Some(a), Some(b)) if a != b);
    if before.mtime != after.mtime || size_changed {
        anyhow::bail!("remote changed while downloading {remote_path}");
    }

    Ok(hash_download(bytes, before.mtime, want_bytes))
}

/// Servers known not to support MDTM retain the historical always-hash
/// fallback. The single RETR is intentionally marked unvalidated through
/// `pre_download: None`: no stable metadata snapshot can be claimed when
/// there is nothing observable to bracket it.
fn download_unvalidated<R: RemoteFileRetrieval>(
    remote: &mut R,
    remote_path: &str,
    want_bytes: bool,
) -> Result<RemoteHash> {
    let bytes = remote
        .download(remote_path)
        .with_context(|| format!("downloading {remote_path}"))?;
    Ok(hash_download(bytes, Utc::now(), want_bytes))
}

fn hash_download(bytes: Vec<u8>, mtime: DateTime<Utc>, want_bytes: bool) -> RemoteHash {
    let sha256 = hash_bytes(&bytes);
    let size = bytes.len() as u64;
    RemoteHash {
        sha256,
        size,
        mtime,
        from_cache: false,
        bytes: if want_bytes { Some(bytes) } else { None },
        pre_download: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FileRecord;
    use chrono::TimeZone;
    use std::collections::VecDeque;

    struct ScriptedRetrieval {
        mtimes: VecDeque<Option<DateTime<Utc>>>,
        sizes: VecDeque<Option<u64>>,
        payload: Option<Vec<u8>>,
        events: Vec<&'static str>,
    }

    impl ScriptedRetrieval {
        fn new(
            mtimes: impl IntoIterator<Item = Option<DateTime<Utc>>>,
            sizes: impl IntoIterator<Item = Option<u64>>,
            payload: &[u8],
        ) -> Self {
            Self {
                mtimes: mtimes.into_iter().collect(),
                sizes: sizes.into_iter().collect(),
                payload: Some(payload.to_vec()),
                events: Vec::new(),
            }
        }

        fn assert_full_bracket(&self) {
            assert_eq!(self.events, ["MDTM", "SIZE", "RETR", "MDTM", "SIZE"]);
            assert_eq!(
                self.events.iter().filter(|event| **event == "RETR").count(),
                1
            );
        }
    }

    impl RemoteFileRetrieval for ScriptedRetrieval {
        fn mtime(&mut self, _remote_path: &str) -> Result<DateTime<Utc>> {
            self.events.push("MDTM");
            self.mtimes
                .pop_front()
                .expect("scripted MDTM call")
                .ok_or_else(|| anyhow::anyhow!("MDTM unsupported"))
        }

        fn size(&mut self, _remote_path: &str) -> Result<u64> {
            self.events.push("SIZE");
            self.sizes
                .pop_front()
                .expect("scripted SIZE call")
                .ok_or_else(|| anyhow::anyhow!("SIZE unsupported"))
        }

        fn download(&mut self, _remote_path: &str) -> Result<Vec<u8>> {
            self.events.push("RETR");
            Ok(self.payload.take().expect("exactly one RETR call"))
        }
    }

    fn timestamp(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 14, 0, second).unwrap()
    }

    fn cached_state(rel: &str, bytes: &[u8], mtime: DateTime<Utc>) -> StateFile {
        let mut state = StateFile::default();
        state.files.insert(
            rel.to_string(),
            FileRecord {
                sha256: hash_bytes(bytes),
                size: bytes.len() as u64,
                remote_mtime: mtime,
                last_synced: mtime,
            },
        );
        state
    }

    #[test]
    fn default_supports_mdtm_is_unknown() {
        let s = StateFile::default();
        assert_eq!(s.server_supports_mdtm, None);
    }

    #[test]
    fn known_unsupported_mdtm_uses_one_unvalidated_download() {
        let payload = b"payload from a server without MDTM";
        let mut remote = ScriptedRetrieval::new([], [], payload);
        let mut state = StateFile {
            server_supports_mdtm: Some(false),
            ..StateFile::default()
        };

        let hash = compute_with(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        assert_eq!(remote.events, ["RETR"]);
        assert_eq!(hash.bytes.as_deref(), Some(payload.as_slice()));
        assert_eq!(hash.sha256, hash_bytes(payload));
        assert_eq!(hash.size, payload.len() as u64);
        assert!(hash.pre_download.is_none());
        assert_eq!(state.server_supports_mdtm, Some(false));
    }

    #[test]
    fn cache_miss_hash_brackets_exactly_one_download() {
        let mtime = timestamp(0);
        let payload = b"downloaded remote payload";
        let mut remote = ScriptedRetrieval::new(
            [Some(mtime), Some(mtime)],
            [Some(payload.len() as u64), Some(payload.len() as u64)],
            payload,
        );
        let mut state = StateFile::default();

        let hash = compute_with(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        remote.assert_full_bracket();
        assert_eq!(hash.bytes.as_deref(), Some(payload.as_slice()));
        assert_eq!(hash.sha256, hash_bytes(payload));
        assert_eq!(hash.size, payload.len() as u64);
        assert_eq!(hash.mtime, mtime);
        assert!(!hash.from_cache);
    }

    #[test]
    fn cache_miss_rejects_changed_mtime_after_one_download() {
        let payload = b"raced payload";
        let mut remote = ScriptedRetrieval::new(
            [Some(timestamp(0)), Some(timestamp(1))],
            [Some(payload.len() as u64), Some(payload.len() as u64)],
            payload,
        );

        let error = compute_with(
            &mut remote,
            &mut StateFile::default(),
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap_err();

        remote.assert_full_bracket();
        assert!(
            format!("{error:#}").contains("remote changed while downloading /remote/page.txt"),
            "got: {error:#}"
        );
    }

    #[test]
    fn cache_miss_rejects_changed_available_size_after_one_download() {
        let mtime = timestamp(0);
        let payload = b"size-raced payload";
        let mut remote =
            ScriptedRetrieval::new([Some(mtime), Some(mtime)], [Some(10), Some(11)], payload);

        let error = compute_with(
            &mut remote,
            &mut StateFile::default(),
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap_err();

        remote.assert_full_bracket();
        assert!(
            format!("{error:#}").contains("remote changed while downloading /remote/page.txt"),
            "got: {error:#}"
        );
    }

    #[test]
    fn cache_hit_returns_hash_after_metadata_probe_without_download() {
        let mtime = timestamp(0);
        let payload = b"cached remote payload";
        let mut remote = ScriptedRetrieval::new(
            [Some(mtime)],
            [Some(payload.len() as u64)],
            b"must not download",
        );
        let mut state = cached_state("page.txt", payload, mtime);

        let hash = compute_with(
            &mut remote,
            &mut state,
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        assert_eq!(remote.events, ["MDTM", "SIZE"]);
        assert!(hash.from_cache);
        assert!(hash.bytes.is_none());
        assert_eq!(hash.sha256, hash_bytes(payload));
        assert_eq!(hash.size, payload.len() as u64);
        assert_eq!(hash.mtime, mtime);
    }

    #[test]
    fn cache_miss_allows_unsupported_size_when_mtime_is_stable() {
        let mtime = timestamp(0);
        let payload = b"stable payload without SIZE";
        let mut remote = ScriptedRetrieval::new([Some(mtime), Some(mtime)], [None, None], payload);

        let hash = compute_with(
            &mut remote,
            &mut StateFile::default(),
            "page.txt",
            "/remote/page.txt",
            true,
        )
        .unwrap();

        remote.assert_full_bracket();
        assert_eq!(hash.bytes.as_deref(), Some(payload.as_slice()));
        assert_eq!(hash.sha256, hash_bytes(payload));
        assert_eq!(hash.size, payload.len() as u64);
        assert_eq!(hash.mtime, mtime);
    }
}
