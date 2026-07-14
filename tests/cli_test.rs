use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ferry"))
}

#[test]
fn help_lists_subcommands() {
    let out = bin().arg("--help").output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    for cmd in ["init", "status", "pull", "push", "sync", "rm"] {
        assert!(stdout.contains(cmd), "missing subcommand: {cmd}");
    }
}

#[test]
fn rm_requires_at_least_one_path() {
    // Bare `rm` must refuse rather than delete anything. The distinctive
    // message (not clap's "unrecognized subcommand") proves our own guard
    // fired, and it must happen before any config/connection work.
    let out = bin().arg("rm").output().unwrap();
    assert!(!out.status.success(), "bare rm should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("at least one path"),
        "expected 'at least one path' guard; stderr={stderr}",
    );
}

#[test]
fn rm_rejects_unsafe_paths() {
    // A `..` path must be refused before we touch the server. Provide a valid
    // config so we get past config-load and reach path validation.
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".ferry.toml");
    std::fs::write(
        &cfg_path,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"
[paths]
remote_root = "/"
"#,
    )
    .unwrap();
    let out = bin()
        .args(["rm", "../escape", "--config"])
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(!out.status.success(), "rm ../escape should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing path"),
        "expected 'refusing path' guard; stderr={stderr}",
    );
}

#[test]
fn missing_config_exits_3() {
    // A non-existent config path must surface as exit code 3 (config/auth
    // category) so Zed's tasks.json can prompt the user to fix .ferry.toml
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
