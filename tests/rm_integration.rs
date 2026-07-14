//! Requires Docker. Run with: cargo test --test rm_integration -- --ignored
use std::process::Command;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};
use zed_ftp::ftp::Ftp;

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

fn run_rm(cfg_path: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zed-ftp"));
    cmd.arg("--config").arg(cfg_path).arg("rm");
    for a in args {
        cmd.arg(a);
    }
    cmd.output().unwrap()
}

#[test]
#[ignore]
fn rm_deletes_file_remote_local_and_state() {
    let (host, port, _c) = start_ftp();
    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();

    let bytes: &[u8] = b"delete me\n";
    ftp.upload_bytes("/notes.txt", bytes).unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::write(local_root.join("notes.txt"), bytes).unwrap();

    // Seed a state entry so we can prove rm drops it.
    let state_dir = local_root.join(".zed-ftp");
    std::fs::create_dir_all(&state_dir).unwrap();
    let now = chrono::Utc::now();
    let mut state = zed_ftp::state::StateFile::default();
    state.files.insert(
        "notes.txt".into(),
        zed_ftp::state::FileRecord {
            sha256: "whatever".into(),
            size: bytes.len() as u64,
            remote_mtime: now,
            last_synced: now,
        },
    );
    state.save(&state_dir.join("state.json")).unwrap();

    let cfg_path = write_config(local_root, &host, port);
    let out = run_rm(&cfg_path, &["notes.txt"]);
    assert!(
        out.status.success(),
        "rm failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(ftp.size("/notes.txt").is_err(), "remote file should be gone");
    assert!(!local_root.join("notes.txt").exists(), "local file should be gone");
    let new_state =
        zed_ftp::state::StateFile::load_or_default(&state_dir.join("state.json")).unwrap();
    assert!(!new_state.files.contains_key("notes.txt"), "state entry should be dropped");
}

#[test]
#[ignore]
fn rm_remote_only_file_succeeds() {
    let (host, port, _c) = start_ftp();
    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();
    ftp.upload_bytes("/orphan.txt", b"only on server\n").unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    let cfg_path = write_config(local_root, &host, port);

    let out = run_rm(&cfg_path, &["orphan.txt"]);
    assert!(
        out.status.success(),
        "rm of remote-only file should succeed: stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(ftp.size("/orphan.txt").is_err(), "remote-only file should be gone");
}

#[test]
#[ignore]
fn rm_recursive_clears_subtree_and_removes_dirs() {
    let (host, port, _c) = start_ftp();
    let mut ftp = Ftp::connect(&host, port, "test", "testpw", true).unwrap();

    ftp.mkdir("/site").unwrap();
    ftp.mkdir("/site/sub").unwrap();
    ftp.upload_bytes("/site/a.txt", b"a\n").unwrap();
    ftp.upload_bytes("/site/sub/b.txt", b"b\n").unwrap();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::create_dir_all(local_root.join("site/sub")).unwrap();
    std::fs::write(local_root.join("site/a.txt"), b"a\n").unwrap();
    std::fs::write(local_root.join("site/sub/b.txt"), b"b\n").unwrap();

    let cfg_path = write_config(local_root, &host, port);
    let out = run_rm(&cfg_path, &["site", "--recursive"]);
    assert!(
        out.status.success(),
        "recursive rm failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(ftp.size("/site/a.txt").is_err(), "remote a.txt should be gone");
    assert!(ftp.size("/site/sub/b.txt").is_err(), "remote b.txt should be gone");
    // The emptied directories should be removed too.
    assert!(ftp.list("/site").is_err(), "remote /site should be removed");
    assert!(!local_root.join("site").exists(), "local site/ should be removed");
}

#[test]
#[ignore]
fn rm_on_directory_without_recursive_errors() {
    let (host, port, _c) = start_ftp();

    let workdir = tempfile::tempdir().unwrap();
    let local_root = workdir.path();
    std::fs::create_dir_all(local_root.join("stuff")).unwrap();
    std::fs::write(local_root.join("stuff/x.txt"), b"x\n").unwrap();

    let cfg_path = write_config(local_root, &host, port);
    let out = run_rm(&cfg_path, &["stuff"]);
    assert!(!out.status.success(), "rm of a directory without --recursive should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing directory"),
        "expected 'refusing directory'; stderr={stderr}",
    );
    // The local directory must be left intact.
    assert!(local_root.join("stuff/x.txt").exists(), "local dir should be untouched");
}
