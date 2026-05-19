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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { .. }    => println!("init stub"),
        Cmd::Status         => {
            let cfg = cli.config.unwrap_or_else(|| std::path::PathBuf::from(".zed-ftp.toml"));
            zed_ftp::commands::status::run(&cfg)?;
        }
        Cmd::Pull { .. }    => println!("pull stub"),
        Cmd::Push { .. }    => println!("push stub"),
        Cmd::Sync { .. }    => println!("sync stub"),
    }
    Ok(())
}
