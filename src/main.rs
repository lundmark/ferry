use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ferry", version, about = "FTP sync helper for editors and coding agents")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    #[arg(long, global = true)]
    config: Option<std::path::PathBuf>,
    #[arg(long, short, global = true)]
    verbose: bool,
    /// Preview write-capable commands without changing local, remote, or Ferry state.
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive setup; validates existing local files vs remote.
    Init { #[arg(long)] no_validate: bool },
    /// List a remote directory (connectivity smoke test). PATH is relative
    /// to `paths.remote_root` from the config; empty lists the root itself.
    Ls { path: Option<String> },
    /// Claude Code / Codex PreToolUse hook. Reads a hook envelope on
    /// stdin and pulls the referenced file with a cooldown window.
    /// Always exits 0 so the LLM tool call is never blocked.
    Hook {
        /// Skip the pull if the file was pulled within this many seconds.
        #[arg(long, default_value_t = 3600)]
        cooldown: i64,
    },
    /// Show per-file sync state vs remote.
    Status,
    /// Download remote -> local.
    Pull { paths: Vec<String>, #[arg(long)] force: bool },
    /// Upload local -> remote.
    Push { paths: Vec<String>, #[arg(long)] force: bool },
    /// Pull then push; refuses on conflict unless --force.
    Sync { #[arg(long)] force: bool },
    /// Delete files on the remote server and the local mirror (and drop their
    /// state records). Requires explicit paths; pass --recursive to delete a
    /// directory subtree.
    Rm { paths: Vec<String>, #[arg(long)] recursive: bool },
    /// Check-compile files on the MUD via the UDP compile service. Prints
    /// per-file OK/FAIL and diagnostics; exits non-zero if any failed.
    #[command(alias = "check")]
    Cc { paths: Vec<String> },
}

/// Exit codes (consumed by Zed's `tasks.json`):
/// - 0 success
/// - 1 generic error (default for anything we don't classify)
/// - 2 conflict — refused without `--force`
/// - 3 config or auth problem — Zed should prompt to fix `.ferry.toml`
///   rather than retry blindly
fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let cli = Cli::parse();
    let mode = ferry::commands::ExecutionMode::from_dry_run(cli.dry_run);
    let explicit_config = cli.config.is_some();
    let mut cfg = cli.config.unwrap_or_else(|| default_config_path(&cli.cmd));
    let is_hook = matches!(&cli.cmd, Cmd::Hook { .. });
    let should_load_config = !matches!(&cli.cmd, Cmd::Init { .. })
        && !matches!(&cli.cmd, Cmd::Rm { paths, .. } if paths.is_empty());
    if !is_hook && mode.should_apply() {
        let config_dir = cfg.parent().unwrap_or_else(|| std::path::Path::new("."));
        if let Err(e) = ferry::names::migrate_legacy(config_dir) {
            eprintln!("warning: {e:#}");
        }
        if !explicit_config {
            cfg = ferry::names::config_path_for_read(config_dir);
        }

        if should_load_config {
            match ferry::config::Config::load(&cfg) {
                Ok(config) => {
                    if let Err(e) = ferry::names::migrate_legacy(&config.paths.local_root) {
                        eprintln!("warning: {e:#}");
                    }
                }
                Err(e) => {
                    eprintln!("error: {e:#}");
                    return classify_exit(&e);
                }
            }
        }
    }
    let result: anyhow::Result<()> = match cli.cmd {
        Cmd::Init { no_validate } => ferry::commands::init::run(&cfg, no_validate, mode),
        Cmd::Ls { path } => ferry::commands::ls::run(&cfg, path.as_deref()),
        Cmd::Hook { cooldown } => ferry::commands::hook::run(cooldown, mode),
        Cmd::Status => ferry::commands::status::run(&cfg, mode),
        Cmd::Pull { paths, force } => ferry::commands::pull::run(&cfg, &paths, force, mode),
        Cmd::Push { paths, force } => ferry::commands::push::run(&cfg, &paths, force, mode),
        Cmd::Sync { force } => ferry::commands::sync::run(&cfg, force, mode),
        Cmd::Rm { paths, recursive } => ferry::commands::rm::run(&cfg, &paths, recursive, mode),
        Cmd::Cc { paths } => ferry::commands::cc::run(&cfg, &paths),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            // `{:#}` expands the full anyhow context chain on one line so
            // users see e.g. "config: reading .ferry.toml: No such file…"
            // rather than just the outermost message.
            eprintln!("error: {e:#}");
            classify_exit(&e)
        }
    }
}

fn default_config_path(cmd: &Cmd) -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    default_config_path_at(cmd, &cwd)
}

fn default_config_path_at(cmd: &Cmd, cwd: &std::path::Path) -> std::path::PathBuf {
    if matches!(cmd, Cmd::Init { .. }) {
        return std::path::PathBuf::from(ferry::names::CONFIG_FILE);
    }
    ferry::project::find_config_upward(cwd)
        .map(|location| location.config_path)
        .unwrap_or_else(|| ferry::names::config_path_for_read(cwd))
}

/// Map an anyhow error onto a process exit code. We check both the root
/// error and the entire `.chain()` so an `Exit::*` wrapped by a later
/// `.with_context(...)` still resolves to its specific exit code.
fn classify_exit(e: &anyhow::Error) -> i32 {
    use ferry::Exit;
    // anyhow's downcast_ref looks at the root; if the Exit was wrapped by
    // `.context(...)` later it won't match here, hence the chain walk below.
    if let Some(exit) = e.downcast_ref::<Exit>() {
        return code_for(exit);
    }
    for cause in e.chain() {
        if let Some(exit) = cause.downcast_ref::<Exit>() {
            return code_for(exit);
        }
    }
    1
}

fn code_for(exit: &ferry::Exit) -> i32 {
    use ferry::Exit;
    match exit {
        Exit::Conflict(_) => 2,
        Exit::Config(_) | Exit::Auth(_) => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_default_config_stays_in_cwd() {
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(project.path().join(ferry::names::CONFIG_FILE), "ancestor").unwrap();

        let init = Cmd::Init { no_validate: true };
        assert_eq!(
            default_config_path_at(&init, &nested),
            std::path::PathBuf::from(ferry::names::CONFIG_FILE),
        );
    }
}
