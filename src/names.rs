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
    if current.exists() {
        current
    } else {
        let legacy = dir.join(LEGACY_CONFIG_FILE);
        if legacy.exists() { legacy } else { current }
    }
}

/// One-time, in-place rename of legacy `.zed-ftp` files in `dir` to the current
/// `.ferry` names. Idempotent and non-clobbering: each item is renamed only
/// when the new name is absent and the legacy name is present, so a clean or
/// already-migrated directory is a no-op and a partial migration completes on a
/// later call. Callers that must never fail (e.g. the hook) can ignore the
/// result.
pub fn migrate_legacy(dir: &Path) -> Result<()> {
    let new_cfg = dir.join(CONFIG_FILE);
    let old_cfg = dir.join(LEGACY_CONFIG_FILE);
    if !new_cfg.exists() && old_cfg.exists() {
        std::fs::rename(&old_cfg, &new_cfg)
            .with_context(|| format!("migrating {LEGACY_CONFIG_FILE} -> {CONFIG_FILE}"))?;
        eprintln!("ferry: migrated {LEGACY_CONFIG_FILE} -> {CONFIG_FILE}");
    }

    let new_state = dir.join(STATE_DIR);
    let old_state = dir.join(LEGACY_STATE_DIR);
    if !new_state.exists() && old_state.exists() {
        std::fs::rename(&old_state, &new_state)
            .with_context(|| format!("migrating {LEGACY_STATE_DIR}/ -> {STATE_DIR}/"))?;
        eprintln!("ferry: migrated {LEGACY_STATE_DIR}/ -> {STATE_DIR}/");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_read_path_falls_back_to_current_when_neither_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(config_path_for_read(dir.path()), dir.path().join(CONFIG_FILE));
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
}
