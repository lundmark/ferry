//! Requires Docker. Run with: cargo test --test editor_sync_integration -- --ignored
mod support;

use ferry::commands::file_transfer::{TransferOutcome, TransferStatus};
use ferry::commands::{pull::pull_one, push::push_one, ExecutionMode};
use ferry::error::Exit;
use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use ferry::state::{FileRecord, StateFile};
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
    let mut ftp = Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("nested")).unwrap();
    ftp.upload_bytes(&remote_path("nested/pull.txt"), remote_bytes).unwrap();

    let outcome = pull_one(&config, "nested/pull.txt", false, ExecutionMode::Apply).unwrap();

    assert_eq!(
        outcome,
        TransferOutcome {
            path: "nested/pull.txt".into(),
            status: TransferStatus::Transferred,
        }
    );
    assert_eq!(std::fs::read(local.path().join("nested/pull.txt")).unwrap(), remote_bytes);
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
    let mut ftp = Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
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
    let mut ftp = Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    assert_eq!(ftp.download(&remote_path(rel)).unwrap(), local_bytes);
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
    let mut ftp = Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("nested")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();

    let error = push_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();

    assert!(error.downcast_ref::<Exit>().is_some());
    assert!(format!("{error:#}").contains(rel));
    assert_eq!(ftp.download(&remote_path(rel)).unwrap(), remote_bytes);
}

#[test]
#[ignore]
fn pull_one_transport_error_names_relative_path() {
    let local = tempfile::tempdir().unwrap();
    let rel = "nested/pull-transport.txt";
    let error = pull_one(&unreachable_config(local.path()), rel, false, ExecutionMode::Apply)
        .unwrap_err();

    assert!(format!("{error:#}").contains(rel));
}

#[test]
#[ignore]
fn push_one_transport_error_names_relative_path() {
    let local = tempfile::tempdir().unwrap();
    let rel = "nested/push-transport.txt";
    let error = push_one(&unreachable_config(local.path()), rel, false, ExecutionMode::Apply)
        .unwrap_err();

    assert!(format!("{error:#}").contains(rel));
}
