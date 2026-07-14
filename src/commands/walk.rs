use crate::ftp::Ftp;
use crate::ignored::Matcher;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;

/// Join a remote root with a relative path, trimming any trailing slash on the
/// root so we never produce `//foo`.
pub fn remote_join(root: &str, rel: &str) -> String {
    let root = root.trim_end_matches('/');
    format!("{}/{}", root, rel)
}

/// Normalize and validate a user-supplied path argument into the relative form
/// used as state-file keys: forward slashes, no leading `./`, no trailing `/`.
///
/// Rejects paths that are empty, absolute, or contain a `..` segment — these
/// could escape the sync roots. Shared by `push` and `rm` so both enforce the
/// same containment rule.
pub fn safe_rel(p: &str) -> Result<String> {
    let s = p.replace('\\', "/");
    let rel = s.trim_start_matches("./").to_string();
    if rel.is_empty() || Path::new(&rel).is_absolute() || rel.split('/').any(|c| c == "..") {
        anyhow::bail!(
            "refusing path {p:?}: must be a relative path under local_root with no '..' segments"
        );
    }
    Ok(rel.trim_end_matches('/').to_string())
}

/// Walk the local mirror, populating `out` with relative paths (forward-slash
/// separated). Skips files matched by the ignore matcher and the `.ferry`
/// state directory.
pub fn walk_local(
    root: &Path,
    dir: &Path,
    matcher: &Matcher,
    out: &mut BTreeSet<String>,
) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading local dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry
            .with_context(|| format!("walking local dir {}", dir.display()))?;
        let path = entry.path();
        let is_dir = path.is_dir();
        if matcher.is_ignored(&path, is_dir) {
            continue;
        }
        if is_dir {
            // Skip the state directory itself.
            if path.file_name().and_then(|s| s.to_str()) == Some(crate::names::STATE_DIR) {
                continue;
            }
            walk_local(root, &path, matcher, out)?;
        } else {
            let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
            // normalize separators on windows; not strictly needed on linux but keeps state portable
            #[cfg(windows)]
            let rel = rel.replace('\\', "/");
            out.insert(rel);
        }
    }
    Ok(())
}

/// Walk the remote tree, populating `out` with relative paths beneath `root`.
///
/// Per-directory listing failures are logged to stderr but do not abort the
/// walk. This is defensive against decades-old FTP trees with dangling
/// symlinks, permission-denied subfolders, and other cruft — one bad
/// subdirectory shouldn't kill the whole operation.
///
/// The top-level call still returns Err if the starting directory itself
/// fails to list — that's a real "the target doesn't exist" signal that
/// callers need.
pub fn walk_remote(
    ftp: &mut Ftp,
    root: &str,
    sub: &str,
    out: &mut BTreeSet<String>,
) -> Result<()> {
    let dir = if sub.is_empty() {
        root.trim_end_matches('/').to_string()
    } else {
        format!("{}/{}", root.trim_end_matches('/'), sub)
    };
    walk_remote_inner(ftp, root, sub, &dir, out, /* top_level = */ true)
}

fn walk_remote_inner(
    ftp: &mut Ftp,
    root: &str,
    sub: &str,
    dir: &str,
    out: &mut BTreeSet<String>,
    top_level: bool,
) -> Result<()> {
    let entries = match ftp.list(dir) {
        Ok(e) => e,
        Err(e) if top_level => {
            return Err(e).with_context(|| format!("walking remote dir {dir}"));
        }
        Err(e) => {
            eprintln!("warning: skipping remote dir {dir}: {e:#}");
            return Ok(());
        }
    };
    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        // Server-supplied names must not contain path separators — a
        // malicious or corrupt listing could otherwise steer the walk
        // outside `root`. Skip suspicious entries with a warning.
        if entry.name.contains('/') || entry.name == ".." || entry.name.is_empty() {
            eprintln!("warning: skipping suspicious remote entry {:?} in {dir}", entry.name);
            continue;
        }
        let child_sub = if sub.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", sub, entry.name)
        };
        if entry.is_dir {
            let child_dir = format!("{}/{}", dir.trim_end_matches('/'), entry.name);
            let _ = walk_remote_inner(ftp, root, &child_sub, &child_dir, out, false);
        } else {
            out.insert(child_sub);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_rel_normalizes_and_strips_dot_slash_and_trailing_slash() {
        assert_eq!(safe_rel("./src/x.html").unwrap(), "src/x.html");
        assert_eq!(safe_rel("src/old/").unwrap(), "src/old");
        assert_eq!(safe_rel("notes.txt").unwrap(), "notes.txt");
    }

    #[test]
    fn safe_rel_rejects_empty() {
        assert!(safe_rel("").is_err());
        assert!(safe_rel("./").is_err());
    }

    #[test]
    fn safe_rel_rejects_absolute() {
        assert!(safe_rel("/etc/passwd").is_err());
    }

    #[test]
    fn safe_rel_rejects_parent_segments() {
        assert!(safe_rel("../escape").is_err());
        assert!(safe_rel("a/../../b").is_err());
    }
}
