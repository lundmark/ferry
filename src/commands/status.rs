use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::{hash_bytes, hash_file};
use crate::ignored::Matcher;
use crate::state::{classify, StateFile};
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;

pub fn run(config_path: &Path) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let local_root = cfg.paths.local_root.clone();
    let state_path = local_root.join(".zed-ftp").join("state.json");
    let state = StateFile::load_or_default(&state_path)?;

    let matcher = Matcher::new(&cfg.sync.ignore, &local_root)?;

    let mut ftp = Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )?;

    // Build sets so we know existence without downloading.
    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    walk_local(&local_root, &local_root, &matcher, &mut local_paths)?;
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    walk_remote(&mut ftp, &cfg.paths.remote_root, "", &mut remote_paths)?;

    let mut all: BTreeSet<&String> = local_paths.iter().chain(remote_paths.iter()).collect();
    for k in state.files.keys() {
        all.insert(k);
    }

    for rel in all {
        let on_local = local_paths.contains(rel);
        let on_remote = remote_paths.contains(rel);
        if !on_local && !on_remote {
            // stale state entry — classify would unreachable. Report and skip.
            println!("{:>14}\t{}", "Stale", rel);
            continue;
        }
        let local_hash = if on_local {
            Some(hash_file(&local_root.join(rel))?)
        } else {
            None
        };
        let remote_hash = if on_remote {
            let remote_path = remote_join(&cfg.paths.remote_root, rel);
            let bytes = ftp.download(&remote_path)?;
            Some(hash_bytes(&bytes))
        } else {
            None
        };
        let known = state.files.get(rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash.as_deref(), known);
        println!("{:>14}\t{}", format!("{:?}", st), rel);
    }

    Ok(())
}

fn remote_join(root: &str, rel: &str) -> String {
    let root = root.trim_end_matches('/');
    format!("{}/{}", root, rel)
}

fn walk_local(
    root: &Path,
    dir: &Path,
    matcher: &Matcher,
    out: &mut BTreeSet<String>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
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

fn walk_remote(
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
    for entry in ftp.list(&dir)? {
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
