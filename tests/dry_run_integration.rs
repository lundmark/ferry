//! Requires Docker. Run with: cargo test --test dry_run_integration -- --ignored
mod support;

use std::io::Write;
use std::process::{Command, Stdio};

use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;

#[test]
#[ignore = "requires Docker"]
fn push_file_dry_run_does_not_upload_or_write_state() {
    let fixture = support::start_ftp();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("new.txt"), b"local only\n").unwrap();
    let config = support::write_config(dir.path(), &fixture);

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&config)
        .args(["push", "new.txt", "--dry-run"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("would push new.txt"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );

    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    assert!(ftp.size(&support::remote_path("new.txt")).is_err());
    assert!(!dir.path().join(".ferry/state.json").exists());

    let _fixture_guard = &fixture.container;
}

#[test]
#[ignore = "requires Docker"]
fn pull_file_dry_run_does_not_create_local_file_or_state() {
    let fixture = support::start_ftp();
    let remote_bytes = b"remote only\n";
    let rel = "remote-only.txt";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&support::remote_path(rel), remote_bytes)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let config = support::write_config(dir.path(), &fixture);

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&config)
        .args(["pull", rel, "--dry-run"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("would pull {rel}")),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(!dir.path().join(rel).exists());
    assert!(!dir.path().join(".ferry/state.json").exists());
    assert_eq!(
        ftp.download(&support::remote_path(rel)).unwrap(),
        remote_bytes
    );

    let _fixture_guard = &fixture.container;
}

#[test]
#[ignore = "requires Docker"]
fn hook_dry_run_does_not_pull_or_save_state() {
    let fixture = support::start_ftp();
    let remote_bytes = b"hook remote only\n";
    let rel = "hook-target.txt";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&support::remote_path(rel), remote_bytes)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    support::write_config(dir.path(), &fixture);
    let target = dir.path().join(rel);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["hook", "--cooldown", "0", "--dry-run"])
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
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!("ferry hook: would pull {rel}")),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(!target.exists());
    assert!(!dir.path().join(".ferry/state.json").exists());
    assert_eq!(
        ftp.download(&support::remote_path(rel)).unwrap(),
        remote_bytes
    );

    let _fixture_guard = &fixture.container;
}

#[test]
#[ignore = "requires Docker"]
fn forced_pull_dry_run_previews_overwrite_without_writing() {
    let fixture = support::start_ftp();
    let remote_bytes = b"remote canonical version\n";
    let local_bytes = b"unsynced local edit\n";
    let rel = "edited.txt";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&support::remote_path(rel), remote_bytes)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let local_path = dir.path().join(rel);
    std::fs::write(&local_path, local_bytes).unwrap();
    let state_path = dir.path().join(".ferry/state.json");
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        rel.into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(remote_bytes),
            size: remote_bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_path).unwrap();
    let config = support::write_config(dir.path(), &fixture);
    let local_before = std::fs::read(&local_path).unwrap();
    let state_before = std::fs::read(&state_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&config)
        .args(["pull", rel, "--force", "--dry-run"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!(
            "would overwrite local with remote (--force): {rel}"
        )),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(std::fs::read(&local_path).unwrap(), local_before);
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);
    assert_eq!(
        ftp.download(&support::remote_path(rel)).unwrap(),
        remote_bytes
    );

    let _fixture_guard = &fixture.container;
}

#[test]
#[ignore = "requires Docker"]
fn hook_dry_run_preserves_legacy_names() {
    let fixture = support::start_ftp();
    let remote_bytes = b"legacy hook remote only\n";
    let rel = "legacy-hook-target.txt";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&support::remote_path(rel), remote_bytes)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let current_config = support::write_config(dir.path(), &fixture);
    let legacy_config = dir.path().join(ferry::names::LEGACY_CONFIG_FILE);
    std::fs::rename(&current_config, &legacy_config).unwrap();

    let legacy_state = dir
        .path()
        .join(ferry::names::LEGACY_STATE_DIR)
        .join("state.json");
    let mut state = ferry::state::StateFile::default();
    state.server_supports_mdtm = Some(false);
    state.save(&legacy_state).unwrap();

    let config_before = std::fs::read(&legacy_config).unwrap();
    let state_before = std::fs::read(&legacy_state).unwrap();
    let target = dir.path().join(rel);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .args(["hook", "--cooldown", "0", "--dry-run"])
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
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!("ferry hook: would pull {rel}")),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(std::fs::read(&legacy_config).unwrap(), config_before);
    assert_eq!(std::fs::read(&legacy_state).unwrap(), state_before);
    assert!(!dir.path().join(ferry::names::CONFIG_FILE).exists());
    assert!(!dir.path().join(ferry::names::STATE_DIR).exists());
    assert!(!target.exists());
    assert_eq!(
        ftp.download(&support::remote_path(rel)).unwrap(),
        remote_bytes
    );

    let _fixture_guard = &fixture.container;
}
