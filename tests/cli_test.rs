use std::io::Write;
use std::process::{Command, Stdio};

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

#[test]
fn dry_run_does_not_migrate_legacy_names() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_config = dir.path().join(ferry::names::LEGACY_CONFIG_FILE);
    let legacy_state = dir
        .path()
        .join(ferry::names::LEGACY_STATE_DIR)
        .join("state.json");
    std::fs::write(
        &legacy_config,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
local_root = "."
remote_root = "/"
"#,
    )
    .unwrap();
    let state = ferry::state::StateFile::default();
    state.save(&legacy_state).unwrap();
    let config_before = std::fs::read(&legacy_config).unwrap();
    let state_before = std::fs::read(&legacy_state).unwrap();

    let out = bin()
        .args(["status", "--dry-run"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(3),
        "expected auth exit after loading legacy config; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ftp connect 127.0.0.1:1"),
        "legacy config was not read through; stderr={stderr}",
    );
    assert_eq!(std::fs::read(&legacy_config).unwrap(), config_before);
    assert_eq!(std::fs::read(&legacy_state).unwrap(), state_before);
    assert!(!dir.path().join(ferry::names::CONFIG_FILE).exists());
    assert!(!dir.path().join(ferry::names::STATE_DIR).exists());
}

#[test]
fn hook_dry_run_cooldown_reads_legacy_state_past_empty_current_dir() {
    let project = tempfile::tempdir().unwrap();
    let local_root = project.path().join("mirror");
    std::fs::create_dir(&local_root).unwrap();
    let target = local_root.join("target.txt");
    std::fs::write(&target, b"local bytes\n").unwrap();

    let config_path = project.path().join(ferry::names::CONFIG_FILE);
    std::fs::write(
        &config_path,
        r#"
[connection]
host = "127.0.0.1"
port = 1
user = "u"
password = "p"

[paths]
local_root = "mirror"
remote_root = "/"
"#,
    )
    .unwrap();

    let legacy_state_path = local_root
        .join(ferry::names::LEGACY_STATE_DIR)
        .join("state.json");
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "target.txt".into(),
        ferry::state::FileRecord {
            sha256: "known".into(),
            size: 12,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&legacy_state_path).unwrap();
    let current_state_dir = local_root.join(ferry::names::STATE_DIR);
    std::fs::create_dir(&current_state_dir).unwrap();

    let config_before = std::fs::read(&config_path).unwrap();
    let state_before = std::fs::read(&legacy_state_path).unwrap();
    let target_before = std::fs::read(&target).unwrap();

    let mut child = bin()
        .args(["hook", "--cooldown", "3600", "--dry-run"])
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    serde_json::to_writer(
        child.stdin.as_mut().unwrap(),
        &serde_json::json!({
            "tool_name": "Read",
            "tool_input": {"file_path": target},
        }),
    )
    .unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("within 3600s cooldown, skipping pull"),
        "legacy cooldown state was ignored and FTP was attempted; stderr={stderr}",
    );
    assert!(!stderr.contains("pull failed"), "stderr={stderr}");
    assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
    assert_eq!(std::fs::read(&legacy_state_path).unwrap(), state_before);
    assert_eq!(std::fs::read(&target).unwrap(), target_before);
    assert!(!current_state_dir.join("state.json").exists());
    assert_eq!(std::fs::read_dir(&current_state_dir).unwrap().count(), 0);
}
