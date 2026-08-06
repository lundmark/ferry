//! Single source of truth for the project's on-disk file names, plus one-time
//! migration from the tool's previous name (`zed-ftp`).

use anyhow::{Context, Result};
use std::path::Path;

/// Per-project config file, in the project root.
pub const CONFIG_FILE: &str = ".ferry.toml";
/// Per-project state directory (holds `state.json`), in the project root.
pub const STATE_DIR: &str = ".ferry";
/// The config file name used before the tool was renamed from `zed-ftp`.
pub const LEGACY_CONFIG_FILE: &str = ".zed-ftp.toml";
/// The state directory name used before the rename from `zed-ftp`.
pub const LEGACY_STATE_DIR: &str = ".zed-ftp";

/// Select the current config for reading, falling back to the legacy name only
/// when the current file is absent. This helper never mutates either path.
pub fn config_path_for_read(dir: &Path) -> std::path::PathBuf {
    let current = dir.join(CONFIG_FILE);
    if entry_is_present(&current) {
        current
    } else {
        let legacy = dir.join(LEGACY_CONFIG_FILE);
        if entry_is_present(&legacy) {
            legacy
        } else {
            current
        }
    }
}

/// Whether a directory entry exists without following a symlink target.
/// Errors other than `NotFound` are treated as present so callers preserve the
/// matching directory boundary and let the eventual read report the error.
pub fn entry_is_present(path: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

/// One-time, in-place rename of legacy `.zed-ftp` files in `dir` to the current
/// `.ferry` names. Idempotent and non-clobbering: each item is renamed only
/// when the new name is absent and the legacy name is present, so a clean or
/// already-migrated directory is a no-op and a partial migration completes on a
/// later call. Callers that must never fail (e.g. the hook) can ignore the
/// result.
fn entry_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("checking path entry {}", path.display())),
    }
}

fn move_file_no_replace(source: &Path, destination: &Path) -> Result<()> {
    if entry_metadata(destination)?.is_some() {
        anyhow::bail!(
            "refusing to replace existing destination {}",
            destination.display()
        );
    }
    let source_metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("checking source file {}", source.display()))?;
    if !source_metadata.file_type().is_file() {
        anyhow::bail!("refusing to move non-file source {}", source.display());
    }
    std::fs::hard_link(source, destination)
        .with_context(|| format!("creating destination file {}", destination.display()))?;
    std::fs::remove_file(source)
        .with_context(|| format!("removing migrated source file {}", source.display()))?;
    Ok(())
}

fn require_real_directory(path: &Path, role: &str) -> Result<Option<std::fs::Metadata>> {
    let metadata = entry_metadata(path)?;
    if let Some(metadata) = &metadata {
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "refusing migration through symlinked {role} directory {}",
                path.display()
            );
        }
        if !metadata.is_dir() {
            anyhow::bail!("expected {role} directory at {}", path.display());
        }
    }
    Ok(metadata)
}

pub fn migrate_legacy(dir: &Path) -> Result<()> {
    let new_cfg = dir.join(CONFIG_FILE);
    let old_cfg = dir.join(LEGACY_CONFIG_FILE);
    match (entry_metadata(&old_cfg)?, entry_metadata(&new_cfg)?) {
        (Some(_), Some(metadata)) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "refusing to replace symlinked config destination {}",
                new_cfg.display()
            );
        }
        (Some(_), None) => {
            move_file_no_replace(&old_cfg, &new_cfg)
                .with_context(|| format!("migrating {LEGACY_CONFIG_FILE} -> {CONFIG_FILE}"))?;
            eprintln!("ferry: migrated {LEGACY_CONFIG_FILE} -> {CONFIG_FILE}");
        }
        _ => {}
    }

    let new_state = dir.join(STATE_DIR);
    let old_state = dir.join(LEGACY_STATE_DIR);
    let new_state_metadata = require_real_directory(&new_state, "current state")?;
    let old_state_metadata = require_real_directory(&old_state, "legacy state")?;
    if new_state_metadata.is_none() && old_state_metadata.is_some() {
        std::fs::rename(&old_state, &new_state)
            .with_context(|| format!("migrating {LEGACY_STATE_DIR}/ -> {STATE_DIR}/"))?;
        eprintln!("ferry: migrated {LEGACY_STATE_DIR}/ -> {STATE_DIR}/");
    } else if new_state_metadata.is_some() && old_state_metadata.is_some() {
        let new_state_file = new_state.join("state.json");
        let old_state_file = old_state.join("state.json");
        match (
            entry_metadata(&old_state_file)?,
            entry_metadata(&new_state_file)?,
        ) {
            (Some(_), Some(metadata)) if metadata.file_type().is_symlink() => {
                anyhow::bail!(
                    "refusing to replace symlinked state destination {}",
                    new_state_file.display()
                );
            }
            (Some(_), None) => {
                move_file_no_replace(&old_state_file, &new_state_file).with_context(|| {
                    format!("migrating {LEGACY_STATE_DIR}/state.json -> {STATE_DIR}/state.json")
                })?;
                eprintln!(
                    "ferry: migrated {LEGACY_STATE_DIR}/state.json -> {STATE_DIR}/state.json"
                );
            }
            _ => {}
        }
        let is_empty = std::fs::read_dir(&old_state)
            .with_context(|| format!("checking legacy state directory {}", old_state.display()))?
            .next()
            .transpose()
            .with_context(|| format!("checking legacy state directory {}", old_state.display()))?
            .is_none();
        if is_empty {
            std::fs::remove_dir(&old_state).with_context(|| {
                format!(
                    "removing empty legacy state directory {}",
                    old_state.display()
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_read_path_falls_back_to_current_when_neither_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            config_path_for_read(dir.path()),
            dir.path().join(CONFIG_FILE)
        );
    }

    #[test]
    fn config_read_path_uses_legacy_when_current_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(LEGACY_CONFIG_FILE);
        std::fs::write(&legacy, "legacy").unwrap();

        assert_eq!(config_path_for_read(dir.path()), legacy);
    }

    #[test]
    fn config_read_path_prefers_current_over_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join(CONFIG_FILE);
        let legacy = dir.path().join(LEGACY_CONFIG_FILE);
        std::fs::write(&current, "current").unwrap();
        std::fs::write(&legacy, "legacy").unwrap();

        assert_eq!(config_path_for_read(dir.path()), current);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_current_config_still_precedes_legacy() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join(CONFIG_FILE);
        let legacy = dir.path().join(LEGACY_CONFIG_FILE);
        symlink(dir.path().join("missing.toml"), &current).unwrap();
        std::fs::write(&legacy, "legacy").unwrap();

        assert_eq!(config_path_for_read(dir.path()), current);
    }

    #[test]
    fn migrate_renames_legacy_config_and_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LEGACY_CONFIG_FILE), "cfg").unwrap();
        std::fs::create_dir(dir.path().join(LEGACY_STATE_DIR)).unwrap();
        std::fs::write(
            dir.path().join(LEGACY_STATE_DIR).join("state.json"),
            "state",
        )
        .unwrap();

        migrate_legacy(dir.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(),
            "cfg"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(STATE_DIR).join("state.json")).unwrap(),
            "state"
        );
        assert!(!dir.path().join(LEGACY_CONFIG_FILE).exists());
        assert!(!dir.path().join(LEGACY_STATE_DIR).exists());
    }

    #[test]
    fn migrate_is_noop_in_a_clean_dir() {
        let dir = tempfile::tempdir().unwrap();
        migrate_legacy(dir.path()).unwrap();
        assert!(!dir.path().join(CONFIG_FILE).exists());
        assert!(!dir.path().join(STATE_DIR).exists());
    }

    #[test]
    fn migrate_does_not_clobber_existing_new_names() {
        let dir = tempfile::tempdir().unwrap();
        // Both old and new present: prefer new, leave old untouched.
        std::fs::write(dir.path().join(CONFIG_FILE), "new").unwrap();
        std::fs::write(dir.path().join(LEGACY_CONFIG_FILE), "old").unwrap();

        migrate_legacy(dir.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(CONFIG_FILE)).unwrap(),
            "new"
        );
        assert!(dir.path().join(LEGACY_CONFIG_FILE).exists());
    }

    #[test]
    fn migrate_completes_a_partial_migration() {
        // Config already migrated, state dir still legacy — a later run should
        // finish the job.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "cfg").unwrap();
        std::fs::create_dir(dir.path().join(LEGACY_STATE_DIR)).unwrap();

        migrate_legacy(dir.path()).unwrap();

        assert!(dir.path().join(STATE_DIR).exists());
        assert!(!dir.path().join(LEGACY_STATE_DIR).exists());
    }

    #[test]
    fn migrate_keeps_legacy_state_when_current_state_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join(STATE_DIR).join("state.json");
        let legacy = dir.path().join(LEGACY_STATE_DIR).join("state.json");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&current, "current").unwrap();
        std::fs::write(&legacy, "legacy").unwrap();

        migrate_legacy(dir.path()).unwrap();

        assert_eq!(std::fs::read_to_string(&current).unwrap(), "current");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "legacy");
    }

    #[cfg(unix)]
    #[test]
    fn migration_preserves_dangling_config_destination_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(LEGACY_CONFIG_FILE);
        let current = dir.path().join(CONFIG_FILE);
        std::fs::write(&legacy, "legacy").unwrap();
        symlink(dir.path().join("missing"), &current).unwrap();

        assert!(migrate_legacy(dir.path()).is_err());
        assert!(
            std::fs::symlink_metadata(&current)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "legacy");
    }

    #[cfg(unix)]
    #[test]
    fn migration_preserves_dangling_state_destination_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let current_dir = dir.path().join(STATE_DIR);
        let legacy_state = dir.path().join(LEGACY_STATE_DIR).join("state.json");
        std::fs::create_dir(&current_dir).unwrap();
        std::fs::create_dir_all(legacy_state.parent().unwrap()).unwrap();
        std::fs::write(&legacy_state, "legacy").unwrap();
        let current = current_dir.join("state.json");
        symlink(dir.path().join("missing"), &current).unwrap();

        assert!(migrate_legacy(dir.path()).is_err());
        assert!(
            std::fs::symlink_metadata(&current)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&legacy_state).unwrap(), "legacy");
    }

    #[cfg(unix)]
    #[test]
    fn migration_rejects_current_state_directory_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let legacy_state = dir.path().join(LEGACY_STATE_DIR).join("state.json");
        std::fs::create_dir_all(legacy_state.parent().unwrap()).unwrap();
        std::fs::write(&legacy_state, "legacy").unwrap();
        symlink(outside.path(), dir.path().join(STATE_DIR)).unwrap();

        assert!(migrate_legacy(dir.path()).is_err());
        assert!(!outside.path().join("state.json").exists());
        assert_eq!(std::fs::read_to_string(&legacy_state).unwrap(), "legacy");
    }

    #[cfg(unix)]
    #[test]
    fn migration_rejects_legacy_state_directory_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_state = outside.path().join("state.json");
        std::fs::write(&outside_state, "legacy").unwrap();
        std::fs::create_dir(dir.path().join(STATE_DIR)).unwrap();
        symlink(outside.path(), dir.path().join(LEGACY_STATE_DIR)).unwrap();

        assert!(migrate_legacy(dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(&outside_state).unwrap(), "legacy");
        assert!(
            std::fs::symlink_metadata(dir.path().join(LEGACY_STATE_DIR))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn migration_removes_empty_legacy_directory_when_current_state_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(STATE_DIR)).unwrap();
        std::fs::write(dir.path().join(STATE_DIR).join("state.json"), "current").unwrap();
        std::fs::create_dir(dir.path().join(LEGACY_STATE_DIR)).unwrap();

        migrate_legacy(dir.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(STATE_DIR).join("state.json")).unwrap(),
            "current"
        );
        assert!(!dir.path().join(LEGACY_STATE_DIR).exists());
    }

    #[test]
    fn file_move_refuses_existing_destination_without_changing_either_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        std::fs::write(&source, "source").unwrap();
        std::fs::write(&destination, "destination").unwrap();

        assert!(move_file_no_replace(&source, &destination).is_err());
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "source");
        assert_eq!(
            std::fs::read_to_string(&destination).unwrap(),
            "destination"
        );
    }
}
