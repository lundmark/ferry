//! `zed-ftp ls [PATH]` — minimal remote listing for connectivity smoke tests.
//!
//! Reads only `[connection]` + `[paths]` from `.zed-ftp.toml`; doesn't touch
//! state, doesn't walk local. PATH is optional: empty means list the
//! configured `remote_root`.

use crate::config::Config;
use crate::ftp::Ftp;
use anyhow::Result;
use std::path::Path;

pub fn run(config_path: &Path, sub: Option<&str>) -> Result<()> {
    let cfg = Config::load(config_path)?;

    let mut ftp = Ftp::connect(
        &cfg.connection.host,
        cfg.connection.port,
        &cfg.connection.user,
        &cfg.connection.password,
        cfg.connection.passive,
    )?;

    let dir = match sub {
        None | Some("") => cfg.paths.remote_root.clone(),
        Some(p) => {
            let root = cfg.paths.remote_root.trim_end_matches('/');
            let p = p.trim_start_matches('/');
            if p.is_empty() {
                cfg.paths.remote_root.clone()
            } else {
                format!("{root}/{p}")
            }
        }
    };

    let entries = ftp.list(&dir)?;
    for e in entries {
        let kind = if e.is_dir { 'd' } else { '-' };
        println!(
            "{kind} {size:>10} {mtime}  {name}",
            kind = kind,
            size = e.size,
            mtime = e.modified.format("%Y-%m-%d %H:%M:%S"),
            name = e.name,
        );
    }
    Ok(())
}
