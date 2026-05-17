use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zed-ftp"))
}

#[test]
fn help_lists_subcommands() {
    let out = bin().arg("--help").output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    for cmd in ["init", "status", "pull", "push", "sync"] {
        assert!(stdout.contains(cmd), "missing subcommand: {cmd}");
    }
}
