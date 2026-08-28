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
        // Non-fatal for the same reason as the remote hash below: one file we
        // cannot read must not end the run. walk_local() now filters out
        // non-regular files, so this is defence in depth against anything that
        // becomes unreadable between the walk and the hash.
        let local_hash = if on_local {
            match hash_file(&local_root.join(rel)) {
                Ok(h) => Some(h),
                Err(e) => {
                    eprintln!("warning: cannot read local {rel}: {e:#}");
                    println!("{:>14}\t{}", "Unreadable", rel);
                    continue;
                }
            }
        } else {
            None
        };
        // Use the MDTM/SIZE fast path where possible — status only needs
        // the hash for classification, never the bytes.
        //
        // A remote file we cannot read must NOT abort the run. Some servers
        // list paths they then refuse to serve: the 3Scapes mud lists
        // /all_explorer_rooms at its root, and any download of it answers
        // "550 No such file or directory". With `?` here that single entry
        // ended the whole command — status got through the root-level files
        // and never reported on players/, secure/ or anything below them,
        // which made it useless on that install. One unreadable path is a
        // property of that path, not of the several thousand others.
        let remote_hash = if on_remote {
            let remote_path = remote_join(&cfg.paths.remote_root, rel);
            match remote_hash::compute(&mut ftp, &mut state, rel, &remote_path, false) {
                Ok(h) => Some(h.sha256),
                Err(e) => {
                    eprintln!("warning: cannot read remote {rel}: {e:#}");
                    println!("{:>14}\t{}", "Unreadable", rel);
                    continue;
                }
            }
        } else {
            None
        };
        let known = state.files.get(rel).map(|r| r.sha256.as_str());
        let st = classify(local_hash.as_deref(), remote_hash.as_deref(), known);
        println!("{:>14}\t{}", format!("{:?}", st), rel);
    }

    // In apply mode, persist ONLY the MDTM capability decision so subsequent
    // runs can skip the probe. Dry-run keeps the cache update in memory only.
    //
    // Re-read from disk and carry over just that one field, rather than saving
    // the snapshot this run started with. A full walk of a large tree takes
    // tens of minutes, and writing `state` back wholesale silently REVERTS
    // every per-file record a concurrent `ferry push` updated in the meantime.
    // The next push then reports "conflict (BothChanged)" against its own
    // earlier work, with no real divergence anywhere -- observed exactly that
    // way against a live mudlib. status is a read-only query and has no
    // business authoring file records.
    if mode.should_apply() {
        let mut on_disk = StateFile::load_or_default(&state_path)?;
        on_disk.server_supports_mdtm = state.server_supports_mdtm;
        on_disk.save(&state_path)?;
    }

    Ok(())
}
