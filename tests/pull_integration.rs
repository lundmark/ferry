//! Requires Docker. Run with: cargo test --test pull_integration -- --ignored
use std::process::Command;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;

fn start_ftp() -> (String, u16, Container<GenericImage>) {
    let img = GenericImage::new("delfer/alpine-ftp-server", "latest")
        .with_exposed_port(21.tcp())
        .with_wait_for(WaitFor::message_on_stderr("vsftpd"))
        .with_env_var("USERS", "test|testpw|/home/test");
    let container = img.start().unwrap();
    let port = container.get_host_port_ipv4(21.tcp()).unwrap();
    ("127.0.0.1".into(), port, container)
}

fn write_config(local_root: &std::path::Path, host: &str, port: u16) -> std::path::PathBuf {
    let cfg_path = local_root.join(".ferry.toml");
    let cfg = format!(
        r#"
[connection]
host = "{host}"
port = {port}
user = "test"
password = "testpw"
passive = true

[paths]
local_root = "{root}"
remote_root = "/"
"#,
        host = host,
        port = port,
        root = local_root.display(),
    );
    std::fs::write(&cfg_path, cfg).unwrap();
    cfg_path
}

#[test]
#[ignore]
fn pull_downloads_new_and_remote_changed_files() {
    let (host, port, _c) = start_ftp();

    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();
    let keep_remote: &[u8] = b"only on remote initially\n";
    let update_remote: &[u8] = b"updated remote version\n";
    ftp.upload_bytes("/keep.txt", keep_remote).unwrap();
    ftp.upload_bytes("/update.txt", update_remote).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();

    // Local has only update.txt, matching the previously-known hash.
    let update_local_original: &[u8] = b"original update content\n";
    std::fs::write(local_root.join("update.txt"), update_local_original).unwrap();

    // Seed state so update.txt is classified as RemoteChanged (local == known,
    // remote != known).
    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "update.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(update_local_original),
            size: update_local_original.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &host, port);

    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("pull")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ferry pull failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // keep.txt should now exist locally with the remote contents.
    let keep_local = std::fs::read(local_root.join("keep.txt")).unwrap();
    assert_eq!(keep_local, keep_remote, "keep.txt should be pulled fresh");

    // update.txt should be overwritten with the remote contents.
    let update_local = std::fs::read(local_root.join("update.txt")).unwrap();
    assert_eq!(update_local, update_remote, "update.txt should be overwritten");

    // State should now have entries for both files with the new hashes.
    let new_state =
        ferry::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    let keep_rec = new_state.files.get("keep.txt").expect("keep.txt in state");
    assert_eq!(keep_rec.sha256, hash_bytes(keep_remote));
    assert_eq!(keep_rec.size, keep_remote.len() as u64);
    let upd_rec = new_state.files.get("update.txt").expect("update.txt in state");
    assert_eq!(upd_rec.sha256, hash_bytes(update_remote));
    assert_eq!(upd_rec.size, update_remote.len() as u64);
}

#[test]
#[ignore]
fn pull_refuses_local_changed_without_force_and_obeys_force() {
    let (host, port, _c) = start_ftp();

    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();
    let remote_bytes: &[u8] = b"remote version of file\n";
    ftp.upload_bytes("/edited.txt", remote_bytes).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();

    // Local copy was edited; state hash matches remote, so:
    //   local_hash != known, remote_hash == known  → LocalChanged
    let local_edits: &[u8] = b"local has unsynced edits\n";
    std::fs::write(local_root.join("edited.txt"), local_edits).unwrap();

    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "edited.txt".into(),
        ferry::state::FileRecord {
            // known == remote so classify() sees LocalChanged
            sha256: hash_bytes(remote_bytes),
            size: remote_bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &host, port);

    // Without --force: pull should fail and local file should be untouched.
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("pull")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "pull without --force should exit non-zero on LocalChanged; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let still_local = std::fs::read(local_root.join("edited.txt")).unwrap();
    assert_eq!(
        still_local, local_edits,
        "local edits should be preserved when pull refuses",
    );

    // With --force: pull should overwrite local with remote.
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("pull")
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "pull --force should succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let after_force = std::fs::read(local_root.join("edited.txt")).unwrap();
    assert_eq!(after_force, remote_bytes, "--force should overwrite local");

    let new_state =
        ferry::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    let rec = new_state.files.get("edited.txt").expect("entry present");
    assert_eq!(rec.sha256, hash_bytes(remote_bytes));
}
