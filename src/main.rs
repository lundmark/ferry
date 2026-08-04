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
    let cfg = cli.config.unwrap_or_else(|| {
        if mode.is_dry_run() {
            ferry::names::config_path_for_read(std::path::Path::new("."))
        } else {
            std::path::PathBuf::from(ferry::names::CONFIG_FILE)
        }
    });
    // Auto-migrate legacy `.zed-ftp` config/state to the current `.ferry` names
    // when using the default config location. Best-effort: a migration failure
    // is a warning, not a hard stop — the command below will surface any real
    // "config not found" error itself.
    if !explicit_config && mode.should_apply() {
        if let Err(e) = ferry::names::migrate_legacy(std::path::Path::new(".")) {
            eprintln!("warning: {e:#}");
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
