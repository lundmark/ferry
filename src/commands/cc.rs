use std::path::Path;

use anyhow::Result;

use crate::commands::walk;
use crate::config::Config;
use crate::udp::CompileClient;

/// `ferry cc <paths...>` — check-compile each file on the MUD via the UDP
/// compile service, printing per-file OK/FAIL and diagnostics. Exits non-zero
/// if any file failed to compile or errored in transport.
pub fn run(config_path: &Path, paths: &[String]) -> Result<()> {
    let cfg = Config::load(config_path)?;
    let client = CompileClient::new(&cfg.connection.host, cfg.connection.udp_port)?;

    let mut any_fail = false;
    for p in paths {
        // Normalize the user path (reject absolute / `..`) and map to the remote
        // mudlib path the same way push/rm do. The server also canonicalizes.
        let remote = match walk::safe_rel(p) {
            Ok(rel) => walk::remote_join(&cfg.paths.remote_root, &rel),
            Err(e) => {
                any_fail = true;
                eprintln!("{p}: error: {e:#}");
                continue;
            }
        };

        match client.check(&cfg.connection.user, &cfg.connection.password, &remote) {
            Ok(res) if res.ok => {
                println!("{p}: OK");
                print_diag(&res.diagnostics);
            }
            Ok(res) => {
                any_fail = true;
                println!("{p}: FAIL");
                print_diag(&res.diagnostics);
            }
            Err(e) => {
                any_fail = true;
                eprintln!("{p}: error: {e:#}");
            }
        }
    }

    if any_fail {
        std::process::exit(1);
    }
    Ok(())
}

fn print_diag(diagnostics: &str) {
    for line in diagnostics.lines() {
        if !line.is_empty() {
            println!("  {line}");
        }
    }
}
