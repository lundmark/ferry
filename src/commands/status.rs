use crate::commands::walk::{remote_join, walk_local, walk_remote};
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
