use super::{record_download, stage_local_write};
use crate::commands::file_transfer::{
    RemotePresence, TransferOutcome, TransferStatus, probe_remote_file,
};
use crate::commands::remote_hash::{self, RemoteHash};
use crate::commands::walk::{remote_join, safe_rel};
use crate::commands::{ExecutionMode, state_path_for};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::hash_bytes;
use crate::state::{FileState, StateFile, classify};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalIdentity {
    Missing,
    Present(String),
}

impl LocalIdentity {
    pub fn capture(path: &Path) -> Result<Self> {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(Self::Present(crate::hash::hash_file(path)?)),
            Ok(_) => anyhow::bail!("{} is not a regular file", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemoteFile {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub size: u64,
    pub mtime: DateTime<Utc>,
}

#[derive(Clone, Debug)]
enum PreparedAction {
    Noop(TransferStatus),
    Install(RemoteFile),
}

#[derive(Clone, Debug)]
pub struct PreparedPull {
    config_path: PathBuf,
    local_root: PathBuf,
    local_path: PathBuf,
    relative_path: String,
    expected_local: LocalIdentity,
    action: PreparedAction,
}

pub fn fetch_remote_one(config_path: &Path, rel: &str) -> Result<RemoteFile> {
    let rel = safe_rel(rel).with_context(|| format!("pull {rel}"))?;
    (|| {
        let cfg = Config::load(config_path)?;
        let mut ftp = connect(&cfg)?;
        let remote_path = remote_join(&cfg.paths.remote_root, &rel);
        if probe_remote_file(&mut ftp, &remote_path)? == RemotePresence::Missing {
            anyhow::bail!("remote has no {rel}");
        }

        let bytes = ftp
            .download(&remote_path)
            .with_context(|| format!("downloading {remote_path}"))?;
        let size = bytes.len() as u64;
        let sha256 = hash_bytes(&bytes);
        let mtime = ftp
            .mtime(&remote_path)
            .with_context(|| format!("fetching mtime for {remote_path}"))?;
        Ok(RemoteFile {
            bytes,
            sha256,
            size,
            mtime,
        })
    })()
    .with_context(|| format!("pull {rel}"))
}

pub fn prepare_pull_one(config_path: &Path, rel: &str, force: bool) -> Result<PreparedPull> {
    let rel = safe_rel(rel).with_context(|| format!("pull {rel}"))?;
    (|| {
        let cfg = Config::load(config_path)?;
        let local_root = cfg.paths.local_root.clone();
        let local_path = local_root.join(&rel);
        let expected_local = LocalIdentity::capture(&local_path)?;
        let local_hash = match &expected_local {
            LocalIdentity::Missing => None,
            LocalIdentity::Present(hash) => Some(hash.as_str()),
        };
        let state_path = state_path_for(&local_root, ExecutionMode::Apply);
        let mut state = StateFile::load_or_default(&state_path)?;
        let mut ftp = connect(&cfg)?;
        let remote_path = remote_join(&cfg.paths.remote_root, &rel);
        let remote_exists =
            probe_remote_file(&mut ftp, &remote_path)? == RemotePresence::Present;
        if !remote_exists && local_hash.is_none() {
            anyhow::bail!("neither local nor remote has {rel}");
        }

        let remote_hash = if remote_exists {
            Some(remote_hash::compute(
                &mut ftp,
                &mut state,
                &rel,
                &remote_path,
                true,
            )?)
        } else {
            None
        };
        let remote_hash_str = remote_hash.as_ref().map(|remote| remote.sha256.as_str());
        let known = state.files.get(&rel).map(|record| record.sha256.as_str());
        let file_state = classify(local_hash, remote_hash_str, known);
        let action = match file_state {
            FileState::InSync => PreparedAction::Noop(TransferStatus::Unchanged),
            FileState::LocalOnly => PreparedAction::Noop(TransferStatus::SkippedMissingSource),
            FileState::RemoteOnly | FileState::RemoteChanged => PreparedAction::Install(
                remote_file_for_install(
                    &mut ftp,
                    &remote_path,
                    remote_hash.expect("remote hash set when remote exists"),
                )?,
            ),
            FileState::LocalChanged | FileState::BothChanged | FileState::Untracked => {
                if !force {
                    return Err(crate::error::Exit::Conflict(format!(
                        "conflict ({file_state:?}) on {rel}: local changes present; pass --force to override",
                    ))
                    .into());
                }
                PreparedAction::Install(remote_file_for_install(
                    &mut ftp,
                    &remote_path,
                    remote_hash.expect("remote hash set when remote exists"),
                )?)
            }
        };

        Ok(PreparedPull {
            config_path: config_path.to_path_buf(),
            local_root,
            local_path,
            relative_path: rel.clone(),
            expected_local,
            action,
        })
    })()
    .with_context(|| format!("pull {rel}"))
}

pub fn prepare_force_pull_one(config_path: &Path, rel: &str) -> Result<PreparedPull> {
    let rel = safe_rel(rel).with_context(|| format!("pull {rel}"))?;
    (|| {
        let cfg = Config::load(config_path)?;
        let local_root = cfg.paths.local_root.clone();
        let local_path = local_root.join(&rel);
        let expected_local = LocalIdentity::capture(&local_path)?;
        let remote = fetch_remote_one(config_path, &rel)?;

        Ok::<PreparedPull, anyhow::Error>(PreparedPull {
            config_path: config_path.to_path_buf(),
            local_root,
            local_path,
            relative_path: rel.clone(),
            expected_local,
            action: PreparedAction::Install(remote),
        })
    })()
    .with_context(|| format!("pull {rel}"))
}

pub fn apply_prepared_pull(prepared: PreparedPull, mode: ExecutionMode) -> Result<TransferOutcome> {
    apply_prepared_pull_if(prepared, mode, || true)
}

pub fn apply_prepared_pull_if<F>(
    prepared: PreparedPull,
    mode: ExecutionMode,
    authorize: F,
) -> Result<TransferOutcome>
where
    F: FnOnce() -> bool,
{
    let PreparedPull {
        config_path,
        local_root,
        local_path,
        relative_path,
        expected_local,
        action,
    } = prepared;
    let context_path = relative_path.clone();

    (|| {
        let cfg = Config::load(&config_path)?;
        if cfg.paths.local_root != local_root {
            return Err(crate::error::Exit::Conflict(format!(
                "local root changed while preparing pull for {relative_path}",
            ))
            .into());
        }

        let saved_local = LocalIdentity::capture(&local_path)?;
        if saved_local != expected_local {
            return Err(local_identity_conflict(&relative_path));
        }

        let state_path = state_path_for(&local_root, mode);
        let mut state = StateFile::load_or_default(&state_path)?;
        let (status, staged) = match action {
            PreparedAction::Noop(status) => (status, None),
            PreparedAction::Install(remote) => {
                record_download(
                    &mut state,
                    &relative_path,
                    &remote.sha256,
                    remote.size,
                    remote.mtime,
                );
                let staged = if mode.should_apply() {
                    Some(stage_local_write(&local_path, &remote.bytes)?)
                } else {
                    None
                };
                (TransferStatus::Transferred, staged)
            }
        };
        let outcome = TransferOutcome::new(&relative_path, status);

        if LocalIdentity::capture(&local_path)? != saved_local {
            return Err(local_identity_conflict(&relative_path));
        }

        if !authorize() {
            return Err(crate::error::Exit::Conflict(format!(
                "pull for {relative_path} was cancelled before commit",
            ))
            .into());
        }

        if let Some(staged) = staged {
            staged.commit()?;
            state.save(&state_path)?;
        }
        Ok(outcome)
    })()
    .with_context(|| format!("pull {context_path}"))
}

pub fn pull_one(
    config_path: &Path,
    rel: &str,
    force: bool,
    mode: ExecutionMode,
) -> Result<TransferOutcome> {
    apply_prepared_pull(prepare_pull_one(config_path, rel, force)?, mode)
}

fn connect(cfg: &Config) -> Result<Ftp> {
    Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )
}

fn remote_file_for_install(
    ftp: &mut Ftp,
    remote_path: &str,
    remote_hash: RemoteHash,
) -> Result<RemoteFile> {
    let bytes = match remote_hash.bytes {
        Some(bytes) => bytes,
        None => ftp
            .download(remote_path)
            .with_context(|| format!("downloading {remote_path}"))?,
    };
    let size = bytes.len() as u64;
    let mtime = ftp
        .mtime(remote_path)
        .with_context(|| format!("fetching mtime for {remote_path}"))?;
    Ok(RemoteFile {
        bytes,
        sha256: remote_hash.sha256,
        size,
        mtime,
    })
}

fn local_identity_conflict(rel: &str) -> anyhow::Error {
    crate::error::Exit::Conflict(format!("local file changed while preparing pull for {rel}",))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ExecutionMode;
    use crate::hash::hash_bytes;
    use crate::state::StateFile;
    use chrono::{TimeZone, Utc};
    use std::path::Path;

    fn write_config(path: &Path, local_root: &Path) {
        std::fs::write(
            path,
            format!(
                r#"
[connection]
host = "example.invalid"
user = "test"
password = "test"

[paths]
local_root = "{}"
remote_root = "/remote"
"#,
                local_root.display()
            ),
        )
        .unwrap();
    }

    #[test]
    fn local_identity_distinguishes_missing_and_present_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.txt");

        assert_eq!(
            LocalIdentity::capture(&path).unwrap(),
            LocalIdentity::Missing
        );

        std::fs::write(&path, b"local bytes").unwrap();
        assert_eq!(
            LocalIdentity::capture(&path).unwrap(),
            LocalIdentity::Present(hash_bytes(b"local bytes"))
        );
    }

    #[test]
    fn apply_rejects_a_changed_local_identity_without_writing_state() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, b"prepared local bytes").unwrap();
        write_config(&config_path, &local_root);
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": true
}"#;
        std::fs::write(&state_path, original_state).unwrap();

        let prepared = PreparedPull {
            config_path,
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: b"remote bytes".to_vec(),
                sha256: hash_bytes(b"remote bytes"),
                size: b"remote bytes".len() as u64,
                mtime: Utc.with_ymd_and_hms(2026, 8, 7, 8, 30, 0).unwrap(),
            }),
        };
        std::fs::write(&local_path, b"edited after prepare").unwrap();

        let error = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap_err();

        assert!(
            error
                .downcast_ref::<crate::error::Exit>()
                .is_some_and(|exit| matches!(exit, crate::error::Exit::Conflict(_))),
            "got: {error:#}"
        );
        assert_eq!(std::fs::read(&local_path).unwrap(), b"edited after prepare");
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
    }

    #[test]
    fn apply_installs_bytes_and_records_supplied_remote_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("nested/page.txt");
        let config_path = dir.path().join("ferry.toml");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, b"local bytes").unwrap();
        write_config(&config_path, &local_root);
        let remote_mtime = Utc.with_ymd_and_hms(2026, 8, 7, 9, 45, 0).unwrap();
        let remote_bytes = b"installed remote bytes";
        let remote_sha256 = hash_bytes(remote_bytes);
        let prepared = PreparedPull {
            config_path,
            local_root: local_root.clone(),
            local_path: local_path.clone(),
            relative_path: "nested/page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: remote_bytes.to_vec(),
                sha256: remote_sha256.clone(),
                size: remote_bytes.len() as u64,
                mtime: remote_mtime,
            }),
        };

        let outcome = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap();

        assert_eq!(outcome.path, "nested/page.txt");
        assert_eq!(
            outcome.status,
            crate::commands::file_transfer::TransferStatus::Transferred
        );
        assert_eq!(std::fs::read(&local_path).unwrap(), remote_bytes);
        let state_path = crate::commands::state_path_for(&local_root, ExecutionMode::Apply);
        let state = StateFile::load_or_default(&state_path).unwrap();
        let record = state.files.get("nested/page.txt").unwrap();
        assert_eq!(record.sha256, remote_sha256);
        assert_eq!(record.size, remote_bytes.len() as u64);
        assert_eq!(record.remote_mtime, remote_mtime);
    }
    #[test]
    fn install_denial_calls_authorization_once_and_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let original_local = b"local bytes before denial";
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": true
}"#;
        std::fs::write(&local_path, original_local).unwrap();
        std::fs::write(&state_path, original_state).unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path,
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: b"denied remote bytes".to_vec(),
                sha256: hash_bytes(b"denied remote bytes"),
                size: b"denied remote bytes".len() as u64,
                mtime: Utc.with_ymd_and_hms(2026, 8, 7, 10, 0, 0).unwrap(),
            }),
        };
        let authorization_calls = std::cell::Cell::new(0);

        let error = apply_prepared_pull_if(prepared, ExecutionMode::Apply, || {
            authorization_calls.set(authorization_calls.get() + 1);
            false
        })
        .unwrap_err();

        assert!(error.downcast_ref::<crate::error::Exit>().is_some());
        assert_eq!(authorization_calls.get(), 1);
        assert_eq!(std::fs::read(&local_path).unwrap(), original_local);
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
        assert!(!super::super::tmp_path(&local_path).exists());
    }

    #[test]
    fn noop_denial_calls_authorization_once_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let original_local = b"unchanged local bytes";
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": null
}"#;
        std::fs::write(&local_path, original_local).unwrap();
        std::fs::write(&state_path, original_state).unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path,
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Noop(TransferStatus::Unchanged),
        };
        let authorization_calls = std::cell::Cell::new(0);

        let error = apply_prepared_pull_if(prepared, ExecutionMode::Apply, || {
            authorization_calls.set(authorization_calls.get() + 1);
            false
        })
        .unwrap_err();

        assert!(error.downcast_ref::<crate::error::Exit>().is_some());
        assert_eq!(authorization_calls.get(), 1);
        assert_eq!(std::fs::read(&local_path).unwrap(), original_local);
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
        assert!(!super::super::tmp_path(&local_path).exists());
    }

    #[test]
    fn changed_local_root_rejects_before_authorization() {
        let dir = tempfile::tempdir().unwrap();
        let local_root = dir.path().join("site");
        let changed_root = dir.path().join("other-site");
        let local_path = local_root.join("page.txt");
        let config_path = dir.path().join("ferry.toml");
        let state_path = local_root.join(crate::names::STATE_DIR).join("state.json");
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        let original_local = b"local bytes";
        let original_state = br#"{
  "version": 1,
  "files": {},
  "server_supports_mdtm": false
}"#;
        std::fs::write(&local_path, original_local).unwrap();
        std::fs::write(&state_path, original_state).unwrap();
        write_config(&config_path, &local_root);
        let prepared = PreparedPull {
            config_path: config_path.clone(),
            local_root,
            local_path: local_path.clone(),
            relative_path: "page.txt".to_string(),
            expected_local: LocalIdentity::capture(&local_path).unwrap(),
            action: PreparedAction::Install(RemoteFile {
                bytes: b"remote bytes".to_vec(),
                sha256: hash_bytes(b"remote bytes"),
                size: b"remote bytes".len() as u64,
                mtime: Utc.with_ymd_and_hms(2026, 8, 7, 11, 0, 0).unwrap(),
            }),
        };
        write_config(&config_path, &changed_root);
        let authorization_calls = std::cell::Cell::new(0);

        let error = apply_prepared_pull_if(prepared, ExecutionMode::Apply, || {
            authorization_calls.set(authorization_calls.get() + 1);
            true
        })
        .unwrap_err();

        assert!(error.downcast_ref::<crate::error::Exit>().is_some());
        assert_eq!(authorization_calls.get(), 0);
        assert_eq!(std::fs::read(&local_path).unwrap(), original_local);
        assert_eq!(std::fs::read(&state_path).unwrap(), original_state);
        assert!(!super::super::tmp_path(&local_path).exists());
    }
}
