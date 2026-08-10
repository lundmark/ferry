//! Requires Docker. Run with:
//! cargo test --test scoped_sync_integration -- --ignored --nocapture --test-threads=1
mod support;

use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use ferry::state::{FileRecord, StateFile};
use std::path::Path;
use std::process::{Command, Output};

fn ftp(fixture: &support::FtpFixture) -> Ftp {
    Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap()
}

fn sync(config: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferry"));
    command.arg("--config").arg(config).arg("sync");
    command.args(args).output().unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn state_path(root: &Path) -> std::path::PathBuf {
    root.join(ferry::names::STATE_DIR).join("state.json")
}

fn record(bytes: &[u8]) -> FileRecord {
    let now = chrono::Utc::now();
    FileRecord {
        sha256: hash_bytes(bytes),
        size: bytes.len() as u64,
        remote_mtime: now,
        last_synced: now,
    }
}

#[test]
#[ignore = "requires Docker"]
fn remote_only_selected_directory_downloads_files_and_empty_children() {
    let fixture = support::start_ftp();
    let mut remote = ftp(&fixture);
    remote.mkdir(&support::remote_path("zones")).unwrap();
    remote.mkdir(&support::remote_path("zones/new")).unwrap();
    remote
        .mkdir(&support::remote_path("zones/new/empty"))
        .unwrap();
    remote
        .upload_bytes(&support::remote_path("zones/new/map.txt"), b"remote map")
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let config = support::write_config(root.path(), &fixture);

    let output = sync(&config, &["zones/new"]);

    assert_success(&output);
    assert_eq!(
        std::fs::read(root.path().join("zones/new/map.txt")).unwrap(),
        b"remote map"
    );
    assert!(root.path().join("zones/new/empty").is_dir());
}

#[test]
#[ignore = "requires Docker"]
fn local_only_selected_directory_uploads_files_and_empty_children() {
    let fixture = support::start_ftp();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("assets/new/empty")).unwrap();
    std::fs::write(root.path().join("assets/new/model.txt"), b"local model").unwrap();
    let config = support::write_config(root.path(), &fixture);

    let output = sync(&config, &["assets/new"]);

    assert_success(&output);
    let mut remote = ftp(&fixture);
    assert_eq!(
        remote
            .download(&support::remote_path("assets/new/model.txt"))
            .unwrap(),
        b"local model"
    );
    assert!(
        remote
            .list(&support::remote_path("assets/new/empty"))
            .is_ok()
    );
}

#[test]
#[ignore = "requires Docker"]
fn exact_file_scope_leaves_changed_sibling_untouched() {
    let fixture = support::start_ftp();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("chosen.txt"), b"chosen local").unwrap();
    std::fs::write(root.path().join("sibling.txt"), b"sibling local").unwrap();
    let mut remote = ftp(&fixture);
    remote
        .upload_bytes(&support::remote_path("sibling.txt"), b"sibling remote")
        .unwrap();
    let config = support::write_config(root.path(), &fixture);

    let output = sync(&config, &["chosen.txt"]);

    assert_success(&output);
    assert_eq!(
        remote
            .download(&support::remote_path("chosen.txt"))
            .unwrap(),
        b"chosen local"
    );
    assert_eq!(
        remote
            .download(&support::remote_path("sibling.txt"))
            .unwrap(),
        b"sibling remote"
    );
    assert_eq!(
        std::fs::read(root.path().join("sibling.txt")).unwrap(),
        b"sibling local"
    );
}

#[test]
#[ignore = "requires Docker"]
fn selected_directory_does_not_touch_near_prefix_sibling() {
    let fixture = support::start_ftp();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("area")).unwrap();
    std::fs::create_dir(root.path().join("area-old")).unwrap();
    std::fs::write(root.path().join("area/new.txt"), b"selected").unwrap();
    std::fs::write(root.path().join("area-old/local.txt"), b"near local").unwrap();
    let mut remote = ftp(&fixture);
    remote.mkdir(&support::remote_path("area-old")).unwrap();
    remote
        .upload_bytes(&support::remote_path("area-old/remote.txt"), b"near remote")
        .unwrap();
    let config = support::write_config(root.path(), &fixture);

    let output = sync(&config, &["area"]);

    assert_success(&output);
    assert!(remote.size(&support::remote_path("area/new.txt")).is_ok());
    assert!(
        remote
            .size(&support::remote_path("area-old/local.txt"))
            .is_err()
    );
    assert!(!root.path().join("area-old/remote.txt").exists());
}

#[test]
#[ignore = "requires Docker"]
fn explicit_configured_root_materializes_empty_directories() {
    let fixture = support::start_ftp();
    let project = tempfile::tempdir().unwrap();
    let root = project.path().join("mirror");
    std::fs::create_dir_all(root.join("local-empty/child")).unwrap();
    let mut remote = ftp(&fixture);
    remote.mkdir(&support::remote_path("remote-empty")).unwrap();
    remote
        .mkdir(&support::remote_path("remote-empty/child"))
        .unwrap();
    let generated = support::write_config(&root, &fixture);
    let config = project.path().join("scoped-sync.toml");
    std::fs::rename(generated, &config).unwrap();
    let selected = root.to_str().unwrap();

    let output = sync(&config, &[selected]);

    assert_success(&output);
    assert!(root.join("remote-empty/child").is_dir());
    assert!(
        remote
            .list(&support::remote_path("local-empty/child"))
            .is_ok()
    );
}

#[test]
#[ignore = "requires Docker"]
fn file_directory_mismatch_preserves_both_sides_and_exits_one() {
    let fixture = support::start_ftp();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("clash"), b"local file").unwrap();
    let mut remote = ftp(&fixture);
    remote.mkdir(&support::remote_path("clash")).unwrap();
    remote
        .upload_bytes(&support::remote_path("clash/child.txt"), b"remote child")
        .unwrap();
    let config = support::write_config(root.path(), &fixture);

    let output = sync(&config, &["clash"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read(root.path().join("clash")).unwrap(),
        b"local file"
    );
    assert_eq!(
        remote
            .download(&support::remote_path("clash/child.txt"))
            .unwrap(),
        b"remote child"
    );
}

#[test]
#[ignore = "requires Docker"]
fn file_conflict_exits_two_after_clean_sibling_finishes_and_state_saves() {
    let fixture = support::start_ftp();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("area")).unwrap();
    std::fs::write(root.path().join("area/conflict.txt"), b"local edit").unwrap();
    let mut remote = ftp(&fixture);
    remote.mkdir(&support::remote_path("area")).unwrap();
    remote
        .upload_bytes(&support::remote_path("area/conflict.txt"), b"remote edit")
        .unwrap();
    remote
        .upload_bytes(&support::remote_path("area/clean.txt"), b"clean remote")
        .unwrap();
    let mut state = StateFile::default();
    state
        .files
        .insert("area/conflict.txt".into(), record(b"old synced"));
    state.save(&state_path(root.path())).unwrap();
    let config = support::write_config(root.path(), &fixture);

    let output = sync(&config, &["area"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        std::fs::read(root.path().join("area/clean.txt")).unwrap(),
        b"clean remote"
    );
    let saved = StateFile::load_or_default(&state_path(root.path())).unwrap();
    assert_eq!(
        saved.files["area/clean.txt"].sha256,
        hash_bytes(b"clean remote")
    );
    assert_eq!(
        remote
            .download(&support::remote_path("area/conflict.txt"))
            .unwrap(),
        b"remote edit"
    );
}

#[test]
#[ignore = "requires Docker"]
fn stale_state_only_entry_is_reported_without_deletion() {
    let fixture = support::start_ftp();
    let project = tempfile::tempdir().unwrap();
    let root = project.path().join("mirror");
    std::fs::create_dir(&root).unwrap();
    let mut state = StateFile::default();
    state
        .files
        .insert("gone.txt".into(), record(b"previous bytes"));
    state.save(&state_path(&root)).unwrap();
    let before = std::fs::read(state_path(&root)).unwrap();
    let generated = support::write_config(&root, &fixture);
    let config = project.path().join("scoped-sync.toml");
    std::fs::rename(generated, &config).unwrap();

    let output = sync(&config, &["."]);

    assert_success(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("skip (not on local or remote): gone.txt")
    );
    assert_eq!(std::fs::read(state_path(&root)).unwrap(), before);
}

#[test]
#[ignore = "requires Docker"]
fn force_on_explicit_file_keeps_local_wins_semantics() {
    let fixture = support::start_ftp();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("force.txt"), b"local winner").unwrap();
    let mut remote = ftp(&fixture);
    remote
        .upload_bytes(&support::remote_path("force.txt"), b"remote loser")
        .unwrap();
    let mut state = StateFile::default();
    state
        .files
        .insert("force.txt".into(), record(b"old synced"));
    state.save(&state_path(root.path())).unwrap();
    let config = support::write_config(root.path(), &fixture);

    let output = sync(&config, &["force.txt", "--force"]);

    assert_success(&output);
    assert_eq!(
        remote.download(&support::remote_path("force.txt")).unwrap(),
        b"local winner"
    );
    assert_eq!(
        std::fs::read(root.path().join("force.txt")).unwrap(),
        b"local winner"
    );
}

#[test]
#[ignore = "requires Docker"]
fn bare_sync_keeps_legacy_project_behavior() {
    let fixture = support::start_ftp();
    let project = tempfile::tempdir().unwrap();
    let root = project.path().join("mirror");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("legacy.txt"), b"legacy local").unwrap();
    let generated = support::write_config(&root, &fixture);
    let config = project.path().join("legacy-sync.toml");
    std::fs::rename(generated, &config).unwrap();

    let output = sync(&config, &[]);

    assert_success(&output);
    let mut remote = ftp(&fixture);
    assert_eq!(
        remote
            .download(&support::remote_path("legacy.txt"))
            .unwrap(),
        b"legacy local"
    );
}
