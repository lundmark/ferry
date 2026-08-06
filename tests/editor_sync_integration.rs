//! Docker cases run with: cargo test --test editor_sync_integration -- --ignored
mod support;

use ferry::commands::file_transfer::{TransferOutcome, TransferStatus};
use ferry::commands::{ExecutionMode, pull::pull_one, push::push_one};
use ferry::error::Exit;
use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use ferry::state::{FileRecord, StateFile};
use std::process::Command;
use support::{remote_path, start_ftp, write_config};

fn seed_state(local_root: &std::path::Path, rel: &str, known_bytes: &[u8]) {
    let state_path = local_root.join(".ferry/state.json");
    let now = chrono::Utc::now();
    let mut state = StateFile::default();
    state.files.insert(
        rel.to_string(),
        FileRecord {
            sha256: hash_bytes(known_bytes),
            size: known_bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_path).unwrap();
}

fn unreachable_config(local_root: &std::path::Path) -> std::path::PathBuf {
    let config_path = local_root.join(".ferry.toml");
    std::fs::write(
        &config_path,
        format!(
            "[connection]\nhost = \"127.0.0.1\"\nport = 1\nuser = \"test\"\npassword = \"testpw\"\npassive = true\n\n[paths]\nlocal_root = {:?}\nremote_root = \"/home/test\"\n",
            local_root.display().to_string(),
        ),
    )
    .unwrap();
    config_path
}

#[test]
#[ignore]
fn pull_one_transfers_remote_bytes_without_printing() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let remote_bytes = b"remote bytes\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("nested")).unwrap();
    ftp.upload_bytes(&remote_path("nested/pull.txt"), remote_bytes)
        .unwrap();

    let outcome = pull_one(&config, "nested/pull.txt", false, ExecutionMode::Apply).unwrap();

    assert_eq!(
        outcome,
        TransferOutcome {
            path: "nested/pull.txt".into(),
            status: TransferStatus::Transferred,
        }
    );
    assert_eq!(
        std::fs::read(local.path().join("nested/pull.txt")).unwrap(),
        remote_bytes
    );
    assert_eq!(
        StateFile::load_or_default(&local.path().join(".ferry/state.json"))
            .unwrap()
            .files
            .get("nested/pull.txt")
            .unwrap()
            .sha256,
        hash_bytes(remote_bytes)
    );
}

#[test]
#[ignore]
fn pull_one_conflict_names_relative_path_and_preserves_local_file() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "nested/pull-conflict.txt";
    let known = b"original\n";
    let local_bytes = b"local edit\n";
    let remote_bytes = b"remote edit\n";
    std::fs::create_dir_all(local.path().join("nested")).unwrap();
    std::fs::write(local.path().join(rel), local_bytes).unwrap();
    seed_state(local.path(), rel, known);
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("nested")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();

    let error = pull_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();

    assert!(error.downcast_ref::<Exit>().is_some());
    assert!(format!("{error:#}").contains(rel));
    assert_eq!(std::fs::read(local.path().join(rel)).unwrap(), local_bytes);
}

#[test]
#[ignore]
fn push_one_transfers_local_bytes_without_printing() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "nested/push.txt";
    let local_bytes = b"local bytes\n";
    std::fs::create_dir_all(local.path().join("nested")).unwrap();
    std::fs::write(local.path().join(rel), local_bytes).unwrap();

    let outcome = push_one(&config, rel, false, ExecutionMode::Apply).unwrap();

    assert_eq!(
        outcome,
        TransferOutcome {
            path: rel.into(),
            status: TransferStatus::Transferred,
        }
    );
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    assert_eq!(ftp.download(&remote_path(rel)).unwrap(), local_bytes);
    assert_eq!(
        StateFile::load_or_default(&local.path().join(".ferry/state.json"))
            .unwrap()
            .files
            .get(rel)
            .unwrap()
            .sha256,
        hash_bytes(local_bytes)
    );
}

#[test]
#[ignore]
fn push_one_conflict_names_relative_path_and_preserves_remote_file() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "nested/push-conflict.txt";
    let known = b"original\n";
    let local_bytes = b"local edit\n";
    let remote_bytes = b"remote edit\n";
    std::fs::create_dir_all(local.path().join("nested")).unwrap();
    std::fs::write(local.path().join(rel), local_bytes).unwrap();
    seed_state(local.path(), rel, known);
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("nested")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();

    let error = push_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();

    assert!(error.downcast_ref::<Exit>().is_some());
    assert!(format!("{error:#}").contains(rel));
    assert_eq!(ftp.download(&remote_path(rel)).unwrap(), remote_bytes);
}

#[test]
fn pull_one_transport_error_names_relative_path() {
    let local = tempfile::tempdir().unwrap();
    let rel = "nested/pull-transport.txt";
    let error = pull_one(
        &unreachable_config(local.path()),
        rel,
        false,
        ExecutionMode::Apply,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains(rel));
}

#[test]
fn push_one_transport_error_names_relative_path() {
    let local = tempfile::tempdir().unwrap();
    let rel = "nested/push-transport.txt";
    let error = push_one(
        &unreachable_config(local.path()),
        rel,
        false,
        ExecutionMode::Apply,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains(rel));
}

#[test]
fn pull_one_rejects_an_escaping_relative_path_before_connecting() {
    let local = tempfile::tempdir().unwrap();
    let error = pull_one(
        &unreachable_config(local.path()),
        "../escape",
        false,
        ExecutionMode::Apply,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("../escape"));
    assert!(format!("{error:#}").contains("refusing path"));
}

#[test]
fn push_one_rejects_an_absolute_path_before_connecting() {
    let local = tempfile::tempdir().unwrap();
    let error = push_one(
        &unreachable_config(local.path()),
        "/escape",
        false,
        ExecutionMode::Apply,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("/escape"));
    assert!(format!("{error:#}").contains("refusing path"));
}

#[test]
fn single_file_apis_are_silent() {
    if std::env::var("FERRY_OUTPUT_PROBE").is_ok() {
        if std::env::var("FERRY_OUTPUT_PROBE") == Ok("probe".into()) {
            let local = tempfile::tempdir().unwrap();
            let config = unreachable_config(local.path());
            assert!(pull_one(&config, "../escape", false, ExecutionMode::Apply).is_err());
            assert!(push_one(&config, "../escape", false, ExecutionMode::Apply).is_err());
        }
        return;
    }

    let current = std::env::current_exe().unwrap();
    let run = |mode: &str| {
        Command::new(&current)
            .args(["--exact", "single_file_apis_are_silent", "--nocapture"])
            .env("FERRY_OUTPUT_PROBE", mode)
            .output()
            .unwrap()
    };
    let control = run("control");
    let probe = run("probe");

    assert!(control.status.success());
    assert!(probe.status.success());
    assert_eq!(
        control.stdout, probe.stdout,
        "single-file API wrote to stdout"
    );
    assert_eq!(
        control.stderr, probe.stderr,
        "single-file API wrote to stderr"
    );
}

#[test]
#[ignore]
fn single_file_noop_and_missing_source_outcomes_do_not_write_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "unchanged.txt";
    let bytes = b"unchanged\n";
    std::fs::write(local.path().join(rel), bytes).unwrap();
    seed_state(local.path(), rel, bytes);
    let state_path = local.path().join(".ferry/state.json");
    let state_before = std::fs::read(&state_path).unwrap();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path(rel), bytes).unwrap();
    ftp.upload_bytes(&remote_path("remote-only.txt"), b"remote\n")
        .unwrap();
    std::fs::write(local.path().join("local-only.txt"), b"local\n").unwrap();

    assert_eq!(
        pull_one(&config, rel, false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Unchanged
    );
    assert_eq!(
        push_one(&config, rel, false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Unchanged
    );
    assert_eq!(
        pull_one(&config, "local-only.txt", false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::SkippedMissingSource
    );
    assert_eq!(
        push_one(&config, "remote-only.txt", false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::SkippedMissingSource
    );
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);
}

#[test]
#[ignore]
fn single_file_force_resolves_both_conflict_directions() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let known = b"known\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();

    std::fs::write(local.path().join("pull-force.txt"), b"local edit\n").unwrap();
    seed_state(local.path(), "pull-force.txt", known);
    ftp.upload_bytes(&remote_path("pull-force.txt"), b"remote edit\n")
        .unwrap();
    assert_eq!(
        pull_one(&config, "pull-force.txt", true, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        std::fs::read(local.path().join("pull-force.txt")).unwrap(),
        b"remote edit\n"
    );

    std::fs::write(local.path().join("push-force.txt"), b"local edit\n").unwrap();
    seed_state(local.path(), "push-force.txt", known);
    ftp.upload_bytes(&remote_path("push-force.txt"), b"remote edit\n")
        .unwrap();
    assert_eq!(
        push_one(&config, "push-force.txt", true, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        ftp.download(&remote_path("push-force.txt")).unwrap(),
        b"local edit\n"
    );
}

#[test]
#[ignore]
fn single_file_dry_run_transfers_do_not_mutate_files_or_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let push_rel = "push-preview.txt";
    let pull_rel = "pull-preview.txt";
    std::fs::write(local.path().join(push_rel), b"local preview\n").unwrap();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path(pull_rel), b"remote preview\n")
        .unwrap();

    assert_eq!(
        pull_one(&config, pull_rel, false, ExecutionMode::DryRun)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        push_one(&config, push_rel, false, ExecutionMode::DryRun)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert!(!local.path().join(pull_rel).exists());
    assert!(ftp.size(&remote_path(push_rel)).is_err());
    assert!(!local.path().join(".ferry/state.json").exists());
}
