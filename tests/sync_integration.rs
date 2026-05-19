//! Requires Docker. Run with: cargo test --test sync_integration -- --ignored
use std::process::Command;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zed_ftp::ftp::Ftp;
use zed_ftp::hash::hash_bytes;

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
    let cfg_path = local_root.join(".zed-ftp.toml");
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
fn sync_noop_when_in_sync() {
    let (host, port, _c) = start_ftp();

    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();
    let bytes: &[u8] = b"in sync on both sides\n";
    ftp.upload_bytes("/agree.txt", bytes).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("agree.txt"), bytes).unwrap();

    // Seed state so local == remote == known → InSync.
    let state_dir = local_root.join(".zed-ftp");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = zed_ftp::state::StateFile::default();
    state.files.insert(
        "agree.txt".into(),
        zed_ftp::state::FileRecord {
            sha256: hash_bytes(bytes),
            size: bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();
    let state_before = std::fs::read(state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &host, port);

    let out = Command::new(env!("CARGO_BIN_EXE_zed-ftp"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sync on a clean tree should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Local file unchanged.
    let local_after = std::fs::read(local_root.join("agree.txt")).unwrap();
    assert_eq!(local_after, bytes, "local should be untouched on noop sync");

    // Remote file unchanged.
    let remote_after = ftp.download("/agree.txt").unwrap();
    assert_eq!(remote_after, bytes, "remote should be untouched on noop sync");

    // State should still have the same single entry. We allow re-serialization
    // to change formatting in principle, but since neither files nor
    // server_supports_mdtm changed, the bytes should be identical.
    let state_after = std::fs::read(state_dir.join("state.json")).unwrap();
    assert_eq!(
        state_after, state_before,
        "state file should be byte-identical when nothing changed",
    );
}

#[test]
#[ignore]
fn sync_uploads_local_and_downloads_remote_in_one_pass() {
    let (host, port, _c) = start_ftp();

    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();

    // Two files. For `local-changed.txt`: local diverged, remote == known.
    //                  For `remote-changed.txt`: remote diverged, local == known.
    let local_changed_known: &[u8] = b"original local-changed content\n";
    let local_changed_new: &[u8] = b"locally edited content\n";
    let remote_changed_known: &[u8] = b"original remote-changed content\n";
    let remote_changed_new: &[u8] = b"remote-edited content\n";

    // Seed remote: local-changed.txt still matches known; remote-changed.txt has new content.
    ftp.upload_bytes("/local-changed.txt", local_changed_known).unwrap();
    ftp.upload_bytes("/remote-changed.txt", remote_changed_new).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    // Local: local-changed.txt has new content; remote-changed.txt matches known.
    std::fs::write(local_root.join("local-changed.txt"), local_changed_new).unwrap();
    std::fs::write(local_root.join("remote-changed.txt"), remote_changed_known).unwrap();

    // Seed state so:
    //   local-changed.txt:  known = local_changed_known → local != known, remote == known → LocalChanged
    //   remote-changed.txt: known = remote_changed_known → local == known, remote != known → RemoteChanged
    let state_dir = local_root.join(".zed-ftp");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = zed_ftp::state::StateFile::default();
    state.files.insert(
        "local-changed.txt".into(),
        zed_ftp::state::FileRecord {
            sha256: hash_bytes(local_changed_known),
            size: local_changed_known.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.files.insert(
        "remote-changed.txt".into(),
        zed_ftp::state::FileRecord {
            sha256: hash_bytes(remote_changed_known),
            size: remote_changed_known.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &host, port);

    let out = Command::new(env!("CARGO_BIN_EXE_zed-ftp"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sync should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // Remote should now match the locally-edited local-changed.txt.
    let remote_lc = ftp.download("/local-changed.txt").unwrap();
    assert_eq!(
        remote_lc, local_changed_new,
        "local-changed.txt remote should be overwritten with local content",
    );

    // Local should now match the remote-edited remote-changed.txt.
    let local_rc = std::fs::read(local_root.join("remote-changed.txt")).unwrap();
    assert_eq!(
        local_rc, remote_changed_new,
        "remote-changed.txt local should be overwritten with remote content",
    );

    // State should now reflect the post-sync hashes for both files.
    let new_state =
        zed_ftp::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    let lc_rec = new_state.files.get("local-changed.txt").expect("local-changed in state");
    assert_eq!(lc_rec.sha256, hash_bytes(local_changed_new));
    assert_eq!(lc_rec.size, local_changed_new.len() as u64);
    let rc_rec = new_state.files.get("remote-changed.txt").expect("remote-changed in state");
    assert_eq!(rc_rec.sha256, hash_bytes(remote_changed_new));
    assert_eq!(rc_rec.size, remote_changed_new.len() as u64);
}

#[test]
#[ignore]
fn sync_refuses_both_changed_then_obeys_force() {
    let (host, port, _c) = start_ftp();

    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();

    // Both sides diverged from the known hash.
    let known_bytes: &[u8] = b"originally synced content\n";
    let local_bytes: &[u8] = b"local edited content\n";
    let remote_bytes: &[u8] = b"remote edited content\n";

    ftp.upload_bytes("/conflict.txt", remote_bytes).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("conflict.txt"), local_bytes).unwrap();

    // Seed state with the original "known" hash — neither side matches it now,
    // and they differ from each other → BothChanged.
    let state_dir = local_root.join(".zed-ftp");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = zed_ftp::state::StateFile::default();
    state.files.insert(
        "conflict.txt".into(),
        zed_ftp::state::FileRecord {
            sha256: hash_bytes(known_bytes),
            size: known_bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &host, port);

    // Without --force: sync should fail; neither side should be modified.
    let out = Command::new(env!("CARGO_BIN_EXE_zed-ftp"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "sync without --force should exit non-zero on BothChanged; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let local_after = std::fs::read(local_root.join("conflict.txt")).unwrap();
    assert_eq!(
        local_after, local_bytes,
        "local edits should be preserved when sync refuses",
    );
    let remote_after = ftp.download("/conflict.txt").unwrap();
    assert_eq!(
        remote_after, remote_bytes,
        "remote edits should be preserved when sync refuses",
    );

    // With --force: local should win — sync uploads local over remote.
    let out = Command::new(env!("CARGO_BIN_EXE_zed-ftp"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("sync")
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sync --force should succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let remote_after_force = ftp.download("/conflict.txt").unwrap();
    assert_eq!(
        remote_after_force, local_bytes,
        "--force should overwrite remote with local content",
    );
    // Local should be unchanged (it's the winner).
    let local_after_force = std::fs::read(local_root.join("conflict.txt")).unwrap();
    assert_eq!(local_after_force, local_bytes, "local should still be local");

    let new_state =
        zed_ftp::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    let rec = new_state.files.get("conflict.txt").expect("entry present");
    assert_eq!(rec.sha256, hash_bytes(local_bytes));
    assert_eq!(rec.size, local_bytes.len() as u64);
}
