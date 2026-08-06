use crate::commands::walk::{remote_join, walk_local, walk_remote};
use crate::commands::{ExecutionMode, remote_hash, state_path_for};
use crate::config::Config;
use crate::ftp::Ftp;
use crate::hash::hash_file;
use crate::ignored::Matcher;
use crate::state::{StateFile, classify};
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::Path;

pub fn run(config_path: &Path, mode: ExecutionMode) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let local_root = cfg.paths.local_root.clone();
    let state_path = state_path_for(&local_root, mode);
    let mut state = StateFile::load_or_default(&state_path)?;

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

    // Collect into owned Strings so we can mutably borrow `state` inside
    // the loop (the MDTM cache lives there).
    let mut all: BTreeSet<String> = BTreeSet::new();
    all.extend(local_paths.iter().cloned());
    all.extend(remote_paths.iter().cloned());
    for k in state.files.keys() {
        all.insert(k.clone());
    }

    for rel in &all {
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
        // Use the MDTM/SIZE fast path where possible — status only needs
        // the hash for classification, never the bytes.
        let remote_hash = if on_remote {
            let remote_path = remote_join(&cfg.paths.remote_root, rel);
            Some(remote_hash::compute(&mut ftp, &mut state, rel, &remote_path, false)?.sha256)
        } else {
            None
        };
        let known = state.files.get(rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash.as_deref(), known);
        println!("{:>14}\t{}", format!("{:?}", st), rel);
    }

    // In apply mode, persist the MDTM capability decision so subsequent runs
    // can skip the probe. Dry-run keeps the cache update in memory only.
    if mode.should_apply() {
        state.save(&state_path)?;
    }

    Ok(())
}
