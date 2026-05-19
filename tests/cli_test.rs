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

#[test]
fn missing_config_exits_3() {
    // A non-existent config path must surface as exit code 3 (config/auth
    // category) so Zed's tasks.json can prompt the user to fix .zed-ftp.toml
    // rather than retry. Use a temp-dir path that's guaranteed not to exist.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.toml");
    let out = bin()
        .args(["status", "--config"])
        .arg(&missing)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "expected exit 3 for missing config; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
fn unknown_subcommand_is_non_zero() {
    // clap exits with its own non-zero code on parse errors; we don't try to
    // remap it. Just sanity-check we don't accidentally exit 0.
    let out = bin().args(["bogus"]).output().unwrap();
    assert_ne!(out.status.code(), Some(0));
}
