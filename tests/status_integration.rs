//! Requires Docker. Run with: cargo test --test status_integration -- --ignored
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

#[test]
#[ignore]
fn status_reports_per_file_state() {
    let (host, port, _c) = start_ftp();

    // Seed remote with three files.
    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();
    let in_sync_contents: &[u8] = b"same on both sides\n";
    let remote_changed_contents: &[u8] = b"remote version\n";
    ftp.upload_bytes("/in_sync.txt", in_sync_contents).unwrap();
    ftp.upload_bytes("/remote_changed.txt", remote_changed_contents)
        .unwrap();
    ftp.upload_bytes("/remote_only.txt", b"only on remote\n")
        .unwrap();

    // Local workdir mirroring most of remote, plus a local-only file.
    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("in_sync.txt"), in_sync_contents).unwrap();
    // For "RemoteChanged" we need local == known and remote != known.
    // Set local to "original", store known = hash("original"); remote is "remote version" (differs).
    let original: &[u8] = b"original content\n";
    std::fs::write(local_root.join("remote_changed.txt"), original).unwrap();
    std::fs::write(local_root.join("local_only.txt"), b"only here\n").unwrap();

    // Pre-populate state with known-good entries.
    let state_dir = local_root.join(".ferry");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = ferry::state::StateFile::default();
    state.files.insert(
        "in_sync.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(in_sync_contents),
            size: in_sync_contents.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.files.insert(
        "remote_changed.txt".into(),
        ferry::state::FileRecord {
            sha256: hash_bytes(original),
            size: original.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    // Write the config pointing at the container.
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

    let out = Command::new(env!("CARGO_BIN_EXE_ferry"))
        .arg("--config")
        .arg(&cfg_path)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ferry status failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).unwrap();

    // The output line format is "{:>14}\t{rel}", e.g. "         InSync\tin_sync.txt".
    assert!(
        stdout.contains("InSync") && stdout.contains("in_sync.txt"),
        "expected InSync for in_sync.txt; got:\n{stdout}"
    );
    assert!(
        stdout.contains("RemoteChanged") && stdout.contains("remote_changed.txt"),
        "expected RemoteChanged for remote_changed.txt; got:\n{stdout}"
    );
    assert!(
        stdout.contains("LocalOnly") && stdout.contains("local_only.txt"),
        "expected LocalOnly for local_only.txt; got:\n{stdout}"
    );
    assert!(
        stdout.contains("RemoteOnly") && stdout.contains("remote_only.txt"),
        "expected RemoteOnly for remote_only.txt; got:\n{stdout}"
    );
}
