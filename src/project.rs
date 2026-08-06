use crate::config::Config;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct ProjectLocation {
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug)]
pub struct ResolvedFile {
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
    pub config: Config,
    pub relative_path: String,
}

pub fn find_config_upward(start: &Path) -> Option<ProjectLocation> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };

    loop {
        let config_path = crate::names::config_path_for_read(&dir);
        if crate::names::entry_is_present(&config_path) {
            return Some(ProjectLocation {
                config_dir: dir,
                config_path,
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn resolve_file(path: &Path, migrate_legacy: bool) -> Result<Option<ResolvedFile>> {
    let Some(location) = find_config_upward(path) else {
        return Ok(None);
    };

    if migrate_legacy && let Err(error) = crate::names::migrate_legacy(&location.config_dir) {
        eprintln!(
            "ferry: warning: migrating legacy paths in {}: {error:#}",
            location.config_dir.display()
        );
    }

    let config_path = crate::names::config_path_for_read(&location.config_dir);
    let config = Config::load(&config_path)?;

    if migrate_legacy && let Err(error) = crate::names::migrate_legacy(&config.paths.local_root) {
        eprintln!(
            "ferry: warning: migrating legacy paths in {}: {error:#}",
            config.paths.local_root.display()
        );
    }

    let relative_path = relative_to_local_root(&config.paths.local_root, path)?;
    Ok(Some(ResolvedFile {
        config_dir: location.config_dir,
        config_path,
        config,
        relative_path,
    }))
}

fn canonicalize_path_or_new_file(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let target = std::fs::read_link(path)
                        .with_context(|| format!("reading symlink {}", path.display()))?;
                    let target = if target.is_absolute() {
                        target
                    } else {
                        path.parent()
                            .context("symlink path has no parent")?
                            .join(target)
                    };
                    canonicalize_path_or_new_file(&target)
                }
                Ok(_) => Err(error)
                    .with_context(|| format!("canonicalizing existing path {}", path.display())),
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
                    let parent = path.parent().context("new file path has no parent")?;
                    let name = path.file_name().context("new file path has no file name")?;
                    Ok(canonicalize_path_or_new_file(parent)?.join(name))
                }
                Err(metadata_error) => {
                    Err(metadata_error).with_context(|| format!("checking path {}", path.display()))
                }
            }
        }
        Err(error) => Err(error).with_context(|| format!("canonicalizing file {}", path.display())),
    }
}

pub fn relative_to_local_root(local_root: &Path, path: &Path) -> Result<String> {
    let local_root = local_root
        .canonicalize()
        .with_context(|| format!("canonicalizing local_root {}", local_root.display()))?;
    let path = canonicalize_path_or_new_file(path)?;

    let relative = path.strip_prefix(&local_root).map_err(|_| {
        anyhow::anyhow!(
            "file {} is outside local_root {}",
            path.display(),
            local_root.display()
        )
    })?;
    let relative = relative
        .to_str()
        .context("relative path is not valid UTF-8")?;
    #[cfg(windows)]
    let relative = relative.replace('\\', "/");
    #[cfg(not(windows))]
    let relative = relative.to_owned();
    crate::commands::walk::safe_rel(&relative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::{CONFIG_FILE, LEGACY_CONFIG_FILE, LEGACY_STATE_DIR, STATE_DIR};

    fn write_config(dir: &Path, local_root: &str) {
        std::fs::write(
            dir.join(CONFIG_FILE),
            format!(
                "[connection]\nhost = \"h\"\nuser = \"u\"\npassword = \"p\"\n\
                 [paths]\nlocal_root = \"{local_root}\"\nremote_root = \"/\"\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn nearest_nested_config_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        let inner = outer.join("nested");
        std::fs::create_dir_all(inner.join("mirror")).unwrap();
        write_config(outer, ".");
        write_config(&inner, "mirror");
        let file = inner.join("mirror/file.c");
        std::fs::write(&file, "").unwrap();

        let resolved = resolve_file(&file, false).unwrap().unwrap();
        assert_eq!(resolved.config_dir, inner);
        assert_eq!(resolved.relative_path, "file.c");
    }

    #[test]
    fn maps_file_through_descendant_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror/sub");
        std::fs::create_dir_all(&mirror).unwrap();
        write_config(tmp.path(), "mirror");
        let file = mirror.join("room.c");
        std::fs::write(&file, "").unwrap();

        let resolved = resolve_file(&file, false).unwrap().unwrap();
        assert_eq!(resolved.relative_path, "sub/room.c");
    }

    #[test]
    fn rejects_file_outside_configured_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mirror")).unwrap();
        write_config(tmp.path(), "mirror");
        let file = tmp.path().join("outside.c");
        std::fs::write(&file, "").unwrap();

        let error = resolve_file(&file, false).unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
    }

    #[test]
    fn discovers_legacy_config() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("one/two");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(tmp.path().join(LEGACY_CONFIG_FILE), "legacy").unwrap();

        let location = find_config_upward(&nested).unwrap();
        assert_eq!(location.config_dir, tmp.path());
        assert_eq!(location.config_path, tmp.path().join(LEGACY_CONFIG_FILE));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_current_config_stops_upward_discovery() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        write_config(tmp.path(), ".");
        let current = nested.join(CONFIG_FILE);
        symlink(nested.join("missing.toml"), &current).unwrap();
        let file = nested.join("file.c");
        std::fs::write(&file, "").unwrap();

        let location = find_config_upward(&nested).unwrap();
        assert_eq!(location.config_dir, nested);
        assert_eq!(location.config_path, current);
        let error = resolve_file(&file, false).unwrap_err();
        assert!(error.to_string().contains("reading"));
        assert!(error.to_string().contains(CONFIG_FILE));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_legacy_config_stops_upward_discovery() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        write_config(tmp.path(), ".");
        let legacy = nested.join(LEGACY_CONFIG_FILE);
        symlink(nested.join("missing.toml"), &legacy).unwrap();

        let location = find_config_upward(&nested).unwrap();
        assert_eq!(location.config_dir, nested);
        assert_eq!(location.config_path, legacy);
    }

    #[test]
    fn returns_none_without_a_config() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file.c");
        std::fs::write(&file, "").unwrap();

        assert!(resolve_file(&file, false).unwrap().is_none());
    }

    #[test]
    fn resolves_a_new_file_through_its_existing_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror");
        std::fs::create_dir(&mirror).unwrap();
        write_config(tmp.path(), "mirror");

        let resolved = resolve_file(&mirror.join("new.c"), false).unwrap().unwrap();
        assert_eq!(resolved.relative_path, "new.c");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_from_local_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&mirror).unwrap();
        std::fs::create_dir(&outside).unwrap();
        write_config(tmp.path(), "mirror");
        let escaped = outside.join("escaped.c");
        std::fs::write(&escaped, "").unwrap();
        symlink(&escaped, mirror.join("link.c")).unwrap();

        let error = resolve_file(&mirror.join("link.c"), false).unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_escape_from_local_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&mirror).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let dangling_target = outside.join("new.c");
        symlink(&dangling_target, mirror.join("link.c")).unwrap();

        let error = relative_to_local_root(&mirror, &mirror.join("link.c")).unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
    }
    #[test]
    fn migration_moves_legacy_state_from_descendant_local_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mirror = tmp.path().join("mirror");
        std::fs::create_dir(&mirror).unwrap();
        write_config(tmp.path(), "mirror");
        std::fs::create_dir(mirror.join(STATE_DIR)).unwrap();
        std::fs::create_dir(mirror.join(LEGACY_STATE_DIR)).unwrap();
        let legacy_state = mirror.join(LEGACY_STATE_DIR).join("state.json");
        std::fs::write(&legacy_state, "legacy-state").unwrap();
        let file = mirror.join("file.c");
        std::fs::write(&file, "").unwrap();

        resolve_file(&file, true).unwrap().unwrap();

        assert_eq!(
            std::fs::read_to_string(mirror.join(STATE_DIR).join("state.json")).unwrap(),
            "legacy-state"
        );
        assert!(!legacy_state.exists());
        assert!(!mirror.join(LEGACY_STATE_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_literal_backslash_in_unix_relative_path() {
        use std::os::unix::ffi::OsStringExt;

        let tmp = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"a\\b.c".to_vec());
        let path = tmp.path().join(&name);
        std::fs::write(&path, "").unwrap();

        assert_eq!(relative_to_local_root(tmp.path(), &path).unwrap(), "a\\b.c");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_relative_path() {
        use std::os::unix::ffi::OsStringExt;

        let tmp = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"bad-\xff.c".to_vec());
        let path = tmp.path().join(name);
        std::fs::write(&path, "").unwrap();

        let error = relative_to_local_root(tmp.path(), &path).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }
}
