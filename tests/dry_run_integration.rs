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
fn sync_dry_run_preserves_both_sides_and_state() {
    let fixture = support::start_ftp();
    let local_bytes = b"local sync dry-run bytes\n";
    let remote_bytes = b"remote sync dry-run bytes\n";
    let local_rel = "local-only.txt";
    let remote_rel = "remote-only.txt";

    let dir = tempfile::tempdir().unwrap();
    let local_root = dir.path().join("mirror");
    std::fs::create_dir(&local_root).unwrap();
    let local_path = local_root.join(local_rel);
    let remote_local_path = local_root.join(remote_rel);
    std::fs::write(&local_path, local_bytes).unwrap();

    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&support::remote_path(remote_rel), remote_bytes)
        .unwrap();

    let generated_config = support::write_config(&local_root, &fixture);
    let config = dir.path().join("sync-config.toml");
    std::fs::rename(generated_config, &config).unwrap();
    let state_path = local_root.join(".ferry/state.json");
    assert!(!state_path.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&config)
        .args(["sync", "--dry-run"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("would upload {local_rel}")),
        "stdout={stdout}",
    );
    assert!(
        stdout.contains(&format!("would download {remote_rel}")),
        "stdout={stdout}",
    );

    assert_eq!(std::fs::read(&local_path).unwrap(), local_bytes);
    assert!(
        ftp.size(&support::remote_path(local_rel)).is_err(),
        "local-only file should remain absent remotely",
    );
    assert!(!remote_local_path.exists());
    assert_eq!(
        ftp.download(&support::remote_path(remote_rel)).unwrap(),
        remote_bytes,
    );
    assert!(!state_path.exists());

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

#[test]
#[ignore = "requires Docker"]
fn rm_dry_run_preserves_remote_local_and_state() {
    let fixture = support::start_ftp();
    let rel = "rm-dry-run.txt";
    let bytes = b"rm dry-run bytes\n";

    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&support::remote_path(rel), bytes).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let local_root = dir.path().join("mirror");
    std::fs::create_dir(&local_root).unwrap();
    let local_path = local_root.join(rel);
    std::fs::write(&local_path, bytes).unwrap();

    let state_path = local_root.join(".ferry/state.json");
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        rel.into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(bytes),
            size: bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_path).unwrap();

    let generated_config = support::write_config(&local_root, &fixture);
    let config = dir.path().join("rm-config.toml");
    std::fs::rename(generated_config, &config).unwrap();
    let state_before = std::fs::read(&state_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&config)
        .args(["rm", rel, "--dry-run"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&format!("would delete (remote+local) {rel}")),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert_eq!(std::fs::read(&local_path).unwrap(), bytes);
    assert_eq!(ftp.download(&support::remote_path(rel)).unwrap(), bytes);
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);

    let _fixture_guard = &fixture.container;
}

#[test]
#[ignore = "requires Docker"]
fn recursive_rm_dry_run_preserves_files_and_directories() {
    let fixture = support::start_ftp();
    let dir_rel = "rm-dry-run-tree";
    let root_rel = "rm-dry-run-tree/root.txt";
    let nested_rel = "rm-dry-run-tree/sub/nested.txt";
    let root_bytes = b"recursive root bytes\n";
    let nested_bytes = b"recursive nested bytes\n";

    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&support::remote_path(dir_rel)).unwrap();
    ftp.mkdir(&support::remote_path(&format!("{dir_rel}/sub")))
        .unwrap();
    ftp.upload_bytes(&support::remote_path(root_rel), root_bytes)
        .unwrap();
    ftp.upload_bytes(&support::remote_path(nested_rel), nested_bytes)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let local_root = dir.path().join("mirror");
    std::fs::create_dir_all(local_root.join(format!("{dir_rel}/sub"))).unwrap();
    std::fs::write(local_root.join(root_rel), root_bytes).unwrap();
    std::fs::write(local_root.join(nested_rel), nested_bytes).unwrap();

    let generated_config = support::write_config(&local_root, &fixture);
    let config = dir.path().join("recursive-rm-config.toml");
    std::fs::rename(generated_config, &config).unwrap();
    let state_path = local_root.join(".ferry/state.json");
    assert!(!state_path.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&config)
        .args(["rm", dir_rel, "--recursive", "--dry-run"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for rel in [root_rel, nested_rel] {
        assert!(
            stdout.contains(&format!("would delete (remote+local) {rel}")),
            "stdout={stdout}",
        );
    }
    let lines: Vec<&str> = stdout.lines().collect();
    let nested_dir_line = format!("would remove dir {dir_rel}/sub/");
    let root_dir_line = format!("would remove dir {dir_rel}/");
    let nested_dir_pos = lines
        .iter()
        .position(|line| *line == nested_dir_line)
        .unwrap_or_else(|| panic!("missing nested directory preview; stdout={stdout}"));
    let root_dir_pos = lines
        .iter()
        .position(|line| *line == root_dir_line)
        .unwrap_or_else(|| panic!("missing root directory preview; stdout={stdout}"));
    assert!(
        nested_dir_pos < root_dir_pos,
        "directory previews were not deepest-first; stdout={stdout}",
    );

    assert_eq!(
        std::fs::read(local_root.join(root_rel)).unwrap(),
        root_bytes
    );
    assert_eq!(
        std::fs::read(local_root.join(nested_rel)).unwrap(),
        nested_bytes,
    );
    assert!(local_root.join(dir_rel).is_dir());
    assert!(local_root.join(format!("{dir_rel}/sub")).is_dir());
    assert_eq!(
        ftp.download(&support::remote_path(root_rel)).unwrap(),
        root_bytes,
    );
    assert_eq!(
        ftp.download(&support::remote_path(nested_rel)).unwrap(),
        nested_bytes,
    );
    assert!(ftp.list(&support::remote_path(dir_rel)).is_ok());
    assert!(
        ftp.list(&support::remote_path(&format!("{dir_rel}/sub")))
            .is_ok()
    );
    assert!(!state_path.exists());

    let _fixture_guard = &fixture.container;
}
