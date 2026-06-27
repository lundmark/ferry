use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "zed-ftp", version, about = "FTP sync helper for Zed projects")]
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
    /// Show per-file sync state vs remote.
    Status,
    /// Download remote -> local.
    Pull { paths: Vec<String>, #[arg(long)] force: bool },
    /// Upload local -> remote.
    Push { paths: Vec<String>, #[arg(long)] force: bool },
    /// Pull then push; refuses on conflict unless --force.
    Sync { #[arg(long)] force: bool },
}

/// Exit codes (consumed by Zed's `tasks.json`):
/// - 0 success
/// - 1 generic error (default for anything we don't classify)
/// - 2 conflict — refused without `--force`
/// - 3 config or auth problem — Zed should prompt to fix `.zed-ftp.toml`
///   rather than retry blindly
fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let cli = Cli::parse();
    let cfg = cli.config.unwrap_or_else(|| std::path::PathBuf::from(".zed-ftp.toml"));
    let result: anyhow::Result<()> = match cli.cmd {
        Cmd::Init { no_validate } => zed_ftp::commands::init::run(&cfg, no_validate),
        Cmd::Status => zed_ftp::commands::status::run(&cfg),
        Cmd::Pull { paths, force } => zed_ftp::commands::pull::run(&cfg, &paths, force),
        Cmd::Push { paths, force } => zed_ftp::commands::push::run(&cfg, &paths, force),
        Cmd::Sync { force } => zed_ftp::commands::sync::run(&cfg, force),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            // `{:#}` expands the full anyhow context chain on one line so
            // users see e.g. "config: reading .zed-ftp.toml: No such file…"
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
    use zed_ftp::Exit;
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

fn code_for(exit: &zed_ftp::Exit) -> i32 {
    use zed_ftp::Exit;
    match exit {
        Exit::Conflict(_) => 2,
        Exit::Config(_) | Exit::Auth(_) => 3,
    }
}
