//! Requires Docker. Run with: cargo test --test push_integration -- --ignored
use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use std::process::Command;
use testcontainers::{
    Container, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
};

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
fn push_uploads_new_and_local_changed_files() {
    let (host, port, _c) = start_ftp();

    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();

    // Seed remote: keep.txt matches the local copy (InSync after seeding),
    // update.txt holds the original content that local will diverge from.
    let keep_bytes: &[u8] = b"both sides agree on keep\n";
    let update_remote_original: &[u8] = b"original update content\n";
    ftp.upload_bytes("/keep.txt", keep_bytes).unwrap();
    ftp.upload_bytes("/update.txt", update_remote_original)
        .unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();

    // Local mirror: matching keep.txt, edited update.txt, and a brand-new
    // newfile.txt that the remote has never seen.
    std::fs::write(local_root.join("keep.txt"), keep_bytes).unwrap();
    let update_local_new: &[u8] = b"locally edited update content\n";
    std::fs::write(local_root.join("update.txt"), update_local_new).unwrap();
    let newfile_bytes: &[u8] = b"a fresh local file\n";
    std::fs::write(local_root.join("newfile.txt"), newfile_bytes).unwrap();

    // Seed state so:
    //   keep.txt   → local == remote == known → InSync (no upload expected)
    //   update.txt → local != known, remote == known → LocalChanged (uploads)
    //   newfile.txt → on local only → LocalOnly (uploads)
    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "keep.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(keep_bytes),
            size: keep_bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.files.insert(
        "update.txt".into(),
        ferry::state::FileRecord {
            // known == remote so classify() sees LocalChanged.
            sha256: hash_bytes(update_remote_original),
            size: update_remote_original.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &host, port);

    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("push")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ferry push failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // update.txt on remote should now match the new local content.
    let update_remote_after = ftp.download("/update.txt").unwrap();
    assert_eq!(
        update_remote_after, update_local_new,
        "update.txt remote should be overwritten with new local content",
    );

    // newfile.txt should have been freshly uploaded.
    let newfile_remote_after = ftp.download("/newfile.txt").unwrap();
    assert_eq!(
        newfile_remote_after, newfile_bytes,
        "newfile.txt should be uploaded to remote",
    );

    // keep.txt on remote should be untouched (InSync no-op).
    let keep_remote_after = ftp.download("/keep.txt").unwrap();
    assert_eq!(
        keep_remote_after, keep_bytes,
        "keep.txt should be unchanged"
    );

    // State should reflect the new hashes for the uploaded files.
    let new_state =
        ferry::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    let upd_rec = new_state
        .files
        .get("update.txt")
        .expect("update.txt in state");
    assert_eq!(upd_rec.sha256, hash_bytes(update_local_new));
    assert_eq!(upd_rec.size, update_local_new.len() as u64);
    let new_rec = new_state
        .files
        .get("newfile.txt")
        .expect("newfile.txt in state");
    assert_eq!(new_rec.sha256, hash_bytes(newfile_bytes));
    assert_eq!(new_rec.size, newfile_bytes.len() as u64);
}

#[test]
#[ignore]
fn push_refuses_remote_changed_without_force_and_obeys_force() {
    let (host, port, _c) = start_ftp();

    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();

    // Remote has a newer version of edited.txt that we haven't seen yet.
    let remote_bytes: &[u8] = b"someone else edited this on the server\n";
    ftp.upload_bytes("/edited.txt", remote_bytes).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();

    // Local copy matches the known hash (unchanged since last sync), so:
    //   local_hash == known, remote_hash != known  → RemoteChanged
    let local_bytes: &[u8] = b"original synced content\n";
    std::fs::write(local_root.join("edited.txt"), local_bytes).unwrap();

    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "edited.txt".into(),
        ferry::state::FileRecord {
            // known == local so classify() sees RemoteChanged.
            sha256: hash_bytes(local_bytes),
            size: local_bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &host, port);

    // Without --force: push should fail and the remote file should be untouched.
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("push")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "push without --force should exit non-zero on RemoteChanged; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let still_remote = ftp.download("/edited.txt").unwrap();
    assert_eq!(
        still_remote, remote_bytes,
        "remote edits should be preserved when push refuses",
    );

    // With --force: push should overwrite remote with local.
    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("push")
        .arg("--force")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "push --force should succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let after_force = ftp.download("/edited.txt").unwrap();
    assert_eq!(after_force, local_bytes, "--force should overwrite remote");

    let new_state =
        ferry::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    let rec = new_state.files.get("edited.txt").expect("entry present");
    assert_eq!(rec.sha256, hash_bytes(local_bytes));
}
