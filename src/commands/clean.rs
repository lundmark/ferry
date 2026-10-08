//! `ferry clean` — remove ferry's own leftover temp files from the remote.
//!
//! A push uploads to `ferry-tmp.<32 hex>` and renames that into place; a push
//! that is killed or loses its connection between the two leaves the temp
//! behind, and the MUD's `ccall` then trips over it. This walks the given
//! remote paths (never following symlinks) and deletes only names that are
//! unmistakably ferry's transfer temps and older than `--hours`, so a push
//! running right now is never touched. Nothing else is ever deleted.

use crate::commands::ExecutionMode;
use crate::commands::transfer_temp::is_reserved_remote_transfer_temp;
use crate::commands::walk::{remote_join, safe_rel};
use crate::config::Config;
use crate::ftp::Ftp;
use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use std::path::Path;

/// Default minimum age of a temp file before it counts as abandoned.
pub const DEFAULT_MIN_AGE_HOURS: i64 = 24;

pub fn run(config_path: &Path, paths: &[String], hours: i64, mode: ExecutionMode) -> Result<()> {
    if paths.is_empty() {
        anyhow::bail!("clean needs at least one remote path (use / for everything)");
    }
    let cfg = Config::load(config_path)?;
    let mut ftp = Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )?;
    let cutoff = Utc::now() - Duration::hours(hours.max(0));
    let mut found = 0usize;
    for arg in paths {
        let dir = if arg.trim_matches('/').is_empty() {
            cfg.paths.remote_root.clone()
        } else {
            remote_join(&cfg.paths.remote_root, &safe_rel(arg.trim_end_matches('/'))?)
        };
        found += clean_dir(&mut ftp, &dir, cutoff, mode)
            .with_context(|| format!("cleaning {dir}"))?;
    }
    eprintln!(
        "{found} leftover temp file{} {}",
        if found == 1 { "" } else { "s" },
        if mode.is_dry_run() { "found" } else { "removed" }
    );
    Ok(())
}

/// Clean `dir` and everything below it; returns how many temps it found.
/// A subdirectory that will not list is warned about and skipped.
fn clean_dir(
    ftp: &mut Ftp,
    dir: &str,
    cutoff: chrono::DateTime<Utc>,
    mode: ExecutionMode,
) -> Result<usize> {
    let mut found = 0usize;
    let mut pending = vec![dir.trim_end_matches('/').to_string()];
    let mut top = true;
    while let Some(current) = pending.pop() {
        let target = if current.is_empty() { "/" } else { current.as_str() };
        let mut listed = ftp.list(target);
        // Retry once on a fresh session: a dead one would fail every
        // directory after it (see walk.rs).
        if listed.is_err() && ftp.reconnect().is_ok() {
            listed = ftp.list(target);
        }
        let entries = match listed {
            Ok(entries) => entries,
            Err(e) if top => return Err(e),
            Err(e) => {
                eprintln!("warning: skipping {current}: {e:#}");
                continue;
            }
        };
        top = false;
        for entry in entries {
            // Only plain names: a listing that echoes paths is not trusted
            // to say where a file is.
            if entry.name == "." || entry.name == ".." || entry.name.contains('/') {
                continue;
            }
            let path = format!("{current}/{}", entry.name);
            if entry.is_symlink {
                continue;
            }
            if entry.is_dir {
                pending.push(path);
                continue;
            }
            if !is_reserved_remote_transfer_temp(&entry.name) || entry.modified > cutoff {
                continue;
            }
            found += 1;
            if mode.is_dry_run() {
                println!("would remove {path}");
            } else {
                ftp.rm(&path).with_context(|| format!("removing {path}"))?;
                println!("removed {path}");
            }
        }
    }
    Ok(found)
}
