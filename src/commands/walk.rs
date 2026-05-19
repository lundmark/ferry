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

/// Walk the local mirror, populating `out` with relative paths (forward-slash
/// separated). Skips files matched by the ignore matcher and the `.zed-ftp`
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
            if path.file_name().and_then(|s| s.to_str()) == Some(".zed-ftp") {
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
    let entries = ftp.list(&dir)
        .with_context(|| format!("walking remote dir {dir}"))?;
    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let child_sub = if sub.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", sub, entry.name)
        };
        if entry.is_dir {
            walk_remote(ftp, root, &child_sub, out)?;
        } else {
            out.insert(child_sub);
        }
    }
    Ok(())
}
