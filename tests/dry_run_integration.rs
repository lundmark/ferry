//! Requires Docker. Run with: cargo test --test dry_run_integration -- --ignored
mod support;

use std::process::Command;

use ferry::ftp::Ftp;

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
