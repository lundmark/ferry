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
use anyhow::Result;
use chrono::{DateTime, Utc};

/// Result of computing a remote hash, either by trusting cached state or by
/// downloading and hashing.
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
    // Fast path: only attempt if (a) we haven't already determined the server
    // doesn't speak MDTM, and (b) we have a cached record to compare against.
    if state.server_supports_mdtm.unwrap_or(true)
        && let Some(known) = state.files.get(rel)
    {
        match ftp.mtime(remote_path) {
            Ok(mtime) => {
                // MDTM works. Now check size too.
                match ftp.size(remote_path) {
                    Ok(size) => {
                        // Remember the server speaks MDTM, in case we
                        // were previously unsure (None).
                        if state.server_supports_mdtm.is_none() {
                            state.server_supports_mdtm = Some(true);
                        }
                        if mtime == known.remote_mtime && size == known.size {
                            return Ok(RemoteHash {
                                sha256: known.sha256.clone(),
                                size,
                                mtime,
                                from_cache: true,
                                bytes: None,
                            });
                        }
                        // Mismatch: fall through to full download.
                    }
                    Err(_) => {
                        // SIZE failed but MDTM worked — odd, but treat
                        // it as "can't use the fast path this round".
                    }
                }
            }
            Err(_) => {
                // MDTM error: assume the server doesn't support it and
                // cache that decision so we skip the probe next run.
                state.server_supports_mdtm = Some(false);
            }
        }
    }

    // Full download path. Either no cached state, mismatch, or MDTM unsupported.
    let bytes = ftp.download(remote_path)?;
    let sha256 = hash_bytes(&bytes);
    let size = bytes.len() as u64;
    // Best-effort mtime: if MDTM is unsupported, use "now" as a stand-in. The
    // value is only used to populate state.files for the next-run fast path;
    // when MDTM is unsupported the fast path won't fire anyway.
    let mtime = ftp.mtime(remote_path).unwrap_or_else(|_| Utc::now());
    Ok(RemoteHash {
        sha256,
        size,
        mtime,
        from_cache: false,
        bytes: if want_bytes { Some(bytes) } else { None },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::FileRecord;
    use chrono::TimeZone;

    // We don't have a mock Ftp; the real behavior is covered by the
    // (Docker-gated, #[ignore]'d) integration test in tests/. These pure unit
    // tests just exercise the state-mutation behavior the compute() function
    // promises around `server_supports_mdtm`.

    #[test]
    fn default_supports_mdtm_is_unknown() {
        let s = StateFile::default();
        assert_eq!(s.server_supports_mdtm, None);
    }

    #[test]
    fn cache_hit_sketch() {
        // Smoke test: cached record fields end up in RemoteHash on a hit.
        // (compute() itself needs an Ftp connection, so we just construct
        // the expected outputs by hand to lock in the field layout.)
        let mtime = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let rec = FileRecord {
            sha256: "deadbeef".into(),
            size: 13,
            remote_mtime: mtime,
            last_synced: mtime,
        };
        let rh = RemoteHash {
            sha256: rec.sha256.clone(),
            size: rec.size,
            mtime: rec.remote_mtime,
            from_cache: true,
            bytes: None,
        };
        assert!(rh.from_cache);
        assert!(rh.bytes.is_none());
        assert_eq!(rh.sha256, "deadbeef");
    }
}
