//! Integration tests for `zed-ftp init`. These do NOT require Docker — the
//! basic init flow (Task 14) doesn't touch FTP. Task 15 will add the
//! connect+validate path and gate it on `--no-validate`.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn init_writes_config_and_updates_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".zed-ftp.toml");
    let cwd = dir.path();

    let mut child = Command::new(env!("CARGO_BIN_EXE_zed-ftp"))
        .args(["init", "--no-validate", "--config", cfg_path.to_str().unwrap()])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Answers, in the order the prompts ask:
    // host, port (default 21), user, password, remote_root, local_root (default ".").
    let answers = "ftp.example.com\n\ndeploy\nsecret\n/var/www/site\n\n";
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(answers.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "init did not succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg_text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(cfg_text.contains("ftp.example.com"));
    assert!(cfg_text.contains("user = \"deploy\""));
    assert!(cfg_text.contains("remote_root = \"/var/www/site\""));

    let gi_path = cwd.join(".gitignore");
    let gi = std::fs::read_to_string(&gi_path).unwrap();
    assert!(gi.contains(".zed-ftp.toml"));
    assert!(gi.contains(".zed-ftp/"));
}

#[test]
fn init_refuses_when_config_exists() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".zed-ftp.toml");
    std::fs::write(&cfg_path, "junk").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_zed-ftp"))
        .args(["init", "--no-validate", "--config", cfg_path.to_str().unwrap()])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected non-zero exit");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already exists"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
