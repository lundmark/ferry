use crate::project::{RelativeToRoot, relative_to_local_root_or_root};
use anyhow::{Result, bail};
use std::path::{Component, Path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncScope {
    LegacyProject,
    RootDirectory,
    Path(String),
}

pub fn from_cli_path(local_root: &Path, input: Option<&str>) -> Result<SyncScope> {
    let Some(input) = input else {
        return Ok(SyncScope::LegacyProject);
    };
    if input.is_empty() {
        bail!("sync path must not be empty");
    }

    let input_path = Path::new(input);
    if input_path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        bail!("refusing sync path {input:?}: parent traversal is not allowed");
    }

    let path = if input_path.is_absolute() {
        input_path.to_path_buf()
    } else {
        local_root.join(input_path)
    };
    match relative_to_local_root_or_root(local_root, &path)? {
        RelativeToRoot::Root => Ok(SyncScope::RootDirectory),
        RelativeToRoot::Path(path) => Ok(SyncScope::Path(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_keeps_legacy_project_scope() {
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            from_cli_path(root.path(), None).unwrap(),
            SyncScope::LegacyProject
        );
    }

    #[test]
    fn root_aware_dot_resolves_to_root_directory() {
        let root = tempfile::tempdir().unwrap();

        assert_eq!(
            from_cli_path(root.path(), Some(".")).unwrap(),
            SyncScope::RootDirectory
        );
    }

    #[test]
    fn absolute_root_resolves_to_root_directory() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().to_str().unwrap();

        assert_eq!(
            from_cli_path(root.path(), Some(input)).unwrap(),
            SyncScope::RootDirectory
        );
    }

    #[test]
    fn descendants_use_state_key_form() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();

        assert_eq!(
            from_cli_path(root.path(), Some("nested/file.c")).unwrap(),
            SyncScope::Path("nested/file.c".into())
        );
        assert_eq!(
            from_cli_path(root.path(), nested.to_str()).unwrap(),
            SyncScope::Path("nested".into())
        );
    }

    #[test]
    fn rejects_empty_parent_traversal_and_absolute_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();

        assert!(from_cli_path(&root, Some("")).is_err());
        assert!(from_cli_path(&root, Some("../outside")).is_err());
        assert!(from_cli_path(&root, Some("nested/../outside")).is_err());
        assert!(from_cli_path(&root, outside.to_str()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_relative_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("link")).unwrap();

        assert!(from_cli_path(&root, Some("link/new.c")).is_err());
    }
}
