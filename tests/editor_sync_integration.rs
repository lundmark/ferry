//! Docker cases run with: cargo test --test editor_sync_integration -- --ignored
mod support;

use ferry::commands::file_transfer::{TransferOutcome, TransferStatus};
use ferry::commands::{
    ExecutionMode,
    pull::{apply_prepared_pull, fetch_remote_one, prepare_force_pull_one, pull_one},
    push::push_one,
};
use ferry::error::Exit;
use ferry::ftp::Ftp;
use ferry::hash::hash_bytes;
use ferry::state::{FileRecord, StateFile};
use std::process::Command;
use support::{remote_path, start_ftp, write_config};

fn seed_state(local_root: &std::path::Path, rel: &str, known_bytes: &[u8]) {
    let state_path = local_root.join(".ferry/state.json");
    let now = chrono::Utc::now();
    let mut state = StateFile::load_or_default(&state_path).unwrap();
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

const OUTPUT_PROBE_TOKEN: &str = "ferry-editor-sync-output-v2";
const STDOUT_BEGIN: &[u8] = b"FERRY_API_STDOUT_BEGIN\n";
const STDOUT_END: &[u8] = b"FERRY_API_STDOUT_END\n";
const STDERR_BEGIN: &[u8] = b"FERRY_API_STDERR_BEGIN\n";
const STDERR_END: &[u8] = b"FERRY_API_STDERR_END\n";

fn output_child_mode() -> Option<String> {
    let mode = std::env::var("FERRY_OUTPUT_PROBE").ok()?;
    (std::env::var("FERRY_OUTPUT_PROBE_TOKEN").ok().as_deref() == Some(OUTPUT_PROBE_TOKEN))
        .then_some(mode)
}

fn api_bytes_between(output: &[u8], begin: &[u8], end: &[u8]) -> Vec<u8> {
    let start = output
        .windows(begin.len())
        .position(|window| window == begin)
        .expect("begin marker")
        + begin.len();
    let end_offset = output[start..]
        .windows(end.len())
        .position(|window| window == end)
        .expect("end marker");
    output[start..start + end_offset].to_vec()
}

fn run_output_child(test: &str, mode: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--include-ignored"])
        .env("FERRY_OUTPUT_PROBE", mode)
        .env("FERRY_OUTPUT_PROBE_TOKEN", OUTPUT_PROBE_TOKEN)
        .output()
        .unwrap()
}

fn assert_silent_api_bytes(output: std::process::Output) {
    assert!(output.status.success());
    assert_eq!(
        api_bytes_between(&output.stdout, STDOUT_BEGIN, STDOUT_END),
        Vec::<u8>::new(),
        "single-file API wrote to stdout"
    );
    assert_eq!(
        api_bytes_between(&output.stderr, STDERR_BEGIN, STDERR_END),
        Vec::<u8>::new(),
        "single-file API wrote to stderr"
    );
}

fn emit_markers() {
    print!("{}", String::from_utf8_lossy(STDOUT_BEGIN));
    eprint!("{}", String::from_utf8_lossy(STDERR_BEGIN));
}

fn emit_end_markers() {
    print!("{}", String::from_utf8_lossy(STDOUT_END));
    eprint!("{}", String::from_utf8_lossy(STDERR_END));
}

#[test]
fn single_file_transport_errors_are_silent() {
    if output_child_mode().as_deref() == Some("transport") {
        let local = tempfile::tempdir().unwrap();
        let config = unreachable_config(local.path());
        emit_markers();
        assert!(pull_one(&config, "transport-pull.txt", false, ExecutionMode::Apply).is_err());
        assert!(push_one(&config, "transport-push.txt", false, ExecutionMode::Apply).is_err());
        emit_end_markers();
        return;
    }

    assert_silent_api_bytes(run_output_child(
        "single_file_transport_errors_are_silent",
        "transport",
    ));
}

#[test]
#[ignore]
fn single_file_successful_transfers_are_silent() {
    if output_child_mode().as_deref() == Some("success") {
        let fixture = start_ftp();
        let local = tempfile::tempdir().unwrap();
        let config = write_config(local.path(), &fixture);
        let pull_rel = "output-pull.txt";
        let push_rel = "output-push.txt";
        std::fs::write(local.path().join(push_rel), b"push output probe\n").unwrap();
        let mut ftp =
            Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
        ftp.upload_bytes(&remote_path(pull_rel), b"pull output probe\n")
            .unwrap();

        emit_markers();
        assert_eq!(
            pull_one(&config, pull_rel, false, ExecutionMode::Apply)
                .unwrap()
                .status,
            TransferStatus::Transferred
        );
        assert_eq!(
            push_one(&config, push_rel, false, ExecutionMode::Apply)
                .unwrap()
                .status,
            TransferStatus::Transferred
        );
        emit_end_markers();
        assert_eq!(
            std::fs::read(local.path().join(pull_rel)).unwrap(),
            b"pull output probe\n"
        );
        assert_eq!(
            ftp.download(&remote_path(push_rel)).unwrap(),
            b"push output probe\n"
        );
        return;
    }

    assert_silent_api_bytes(run_output_child(
        "single_file_successful_transfers_are_silent",
        "success",
    ));
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

#[test]
#[ignore]
fn single_file_clean_changed_branches_transfer_and_persist_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let known = b"known\n";
    let remote_changed = b"remote changed\n";
    let local_changed = b"local changed\n";
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();

    std::fs::write(local.path().join("remote-changed.txt"), known).unwrap();
    seed_state(local.path(), "remote-changed.txt", known);
    ftp.upload_bytes(&remote_path("remote-changed.txt"), remote_changed)
        .unwrap();
    assert_eq!(
        pull_one(&config, "remote-changed.txt", false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        std::fs::read(local.path().join("remote-changed.txt")).unwrap(),
        remote_changed
    );

    std::fs::write(local.path().join("local-changed.txt"), local_changed).unwrap();
    seed_state(local.path(), "local-changed.txt", known);
    ftp.upload_bytes(&remote_path("local-changed.txt"), known)
        .unwrap();
    assert_eq!(
        push_one(&config, "local-changed.txt", false, ExecutionMode::Apply)
            .unwrap()
            .status,
        TransferStatus::Transferred
    );
    assert_eq!(
        ftp.download(&remote_path("local-changed.txt")).unwrap(),
        local_changed
    );
    let state = StateFile::load_or_default(&local.path().join(".ferry/state.json")).unwrap();
    assert_eq!(
        state.files["remote-changed.txt"].sha256,
        hash_bytes(remote_changed)
    );
    assert_eq!(
        state.files["local-changed.txt"].sha256,
        hash_bytes(local_changed)
    );
}

#[test]
#[ignore]
fn single_file_untracked_conflict_preserves_both_sides_and_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "untracked.txt";
    let local_bytes = b"local untracked\n";
    let remote_bytes = b"remote untracked\n";
    std::fs::write(local.path().join(rel), local_bytes).unwrap();
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();

    let pull_error = pull_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();
    let push_error = push_one(&config, rel, false, ExecutionMode::Apply).unwrap_err();

    assert!(pull_error.downcast_ref::<Exit>().is_some());
    assert!(push_error.downcast_ref::<Exit>().is_some());
    assert_eq!(std::fs::read(local.path().join(rel)).unwrap(), local_bytes);
    assert_eq!(ftp.download(&remote_path(rel)).unwrap(), remote_bytes);
    assert!(!local.path().join(".ferry/state.json").exists());
}

#[test]
#[ignore]
fn fetch_remote_one_returns_bytes_without_mutating_local_or_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "prepared/fetch-only.txt";
    let local_bytes = b"local bytes must remain exact\n\0";
    let remote_bytes = b"controlled remote payload\n\0\xff";
    let local_path = local.path().join(rel);
    let state_path = local.path().join(".ferry/state.json");
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, local_bytes).unwrap();
    seed_state(local.path(), rel, b"previously synchronized bytes\n");
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("prepared")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();
    let remote_mtime = ftp.mtime(&remote_path(rel)).unwrap();
    let local_before = std::fs::read(&local_path).unwrap();
    let state_before = std::fs::read(&state_path).unwrap();

    let remote = fetch_remote_one(&config, rel).unwrap();

    assert_eq!(remote.bytes, remote_bytes);
    assert_eq!(remote.sha256, hash_bytes(remote_bytes));
    assert_eq!(remote.size, remote_bytes.len() as u64);
    assert_eq!(remote.mtime, remote_mtime);
    assert_eq!(std::fs::read(&local_path).unwrap(), local_before);
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);
}

#[test]
#[ignore]
fn prepared_force_pull_installs_the_fetched_remote_and_updates_state() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "prepared/force-install.txt";
    let equal_rel = "prepared/force-install-equal.txt";
    let local_bytes = b"local bytes before forced install\n\0";
    let remote_bytes = b"fetched remote bytes for forced install\n\0\xff";
    let equal_bytes = b"same bytes still require a forced install\n\0";
    let local_path = local.path().join(rel);
    let equal_local_path = local.path().join(equal_rel);
    let state_path = local.path().join(".ferry/state.json");
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, local_bytes).unwrap();
    std::fs::write(&equal_local_path, equal_bytes).unwrap();
    seed_state(local.path(), rel, local_bytes);
    seed_state(local.path(), equal_rel, equal_bytes);
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("prepared")).unwrap();
    ftp.upload_bytes(&remote_path(rel), remote_bytes).unwrap();
    ftp.upload_bytes(&remote_path(equal_rel), equal_bytes)
        .unwrap();
    let remote_mtime = ftp.mtime(&remote_path(rel)).unwrap();
    let equal_remote_mtime = ftp.mtime(&remote_path(equal_rel)).unwrap();
    let local_before = std::fs::read(&local_path).unwrap();
    let equal_local_before = std::fs::read(&equal_local_path).unwrap();
    let state_before = std::fs::read(&state_path).unwrap();

    let prepared = prepare_force_pull_one(&config, rel).unwrap();
    let equal_prepared = prepare_force_pull_one(&config, equal_rel).unwrap();

    assert_eq!(std::fs::read(&local_path).unwrap(), local_before);
    assert_eq!(
        std::fs::read(&equal_local_path).unwrap(),
        equal_local_before
    );
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);

    let outcome = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap();
    let equal_outcome = apply_prepared_pull(equal_prepared, ExecutionMode::Apply).unwrap();

    assert_eq!(
        outcome,
        TransferOutcome::new(rel, TransferStatus::Transferred)
    );
    assert_eq!(
        equal_outcome,
        TransferOutcome::new(equal_rel, TransferStatus::Transferred)
    );
    assert_eq!(std::fs::read(&local_path).unwrap(), remote_bytes);
    assert_eq!(std::fs::read(&equal_local_path).unwrap(), equal_bytes);
    let state = StateFile::load_or_default(&state_path).unwrap();
    let record = state.files.get(rel).unwrap();
    assert_eq!(record.sha256, hash_bytes(remote_bytes));
    assert_eq!(record.size, remote_bytes.len() as u64);
    assert_eq!(record.remote_mtime, remote_mtime);
    let equal_record = state.files.get(equal_rel).unwrap();
    assert_eq!(equal_record.sha256, hash_bytes(equal_bytes));
    assert_eq!(equal_record.size, equal_bytes.len() as u64);
    assert_eq!(equal_record.remote_mtime, equal_remote_mtime);
}

#[test]
#[ignore]
fn prepared_force_pull_rejects_a_local_change_after_preparation() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "prepared/stale-local.txt";
    let local_path = local.path().join(rel);
    let state_path = local.path().join(".ferry/state.json");
    let edited_bytes = b"edited after preparation\n\0";
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, b"local before preparation\n").unwrap();
    seed_state(local.path(), rel, b"previously synchronized bytes\n");
    let mut ftp =
        Ftp::connect(&fixture.host, fixture.control_port, "test", "testpw", true).unwrap();
    ftp.mkdir(&remote_path("prepared")).unwrap();
    ftp.upload_bytes(&remote_path(rel), b"remote fetched during preparation\n")
        .unwrap();
    let prepared = prepare_force_pull_one(&config, rel).unwrap();
    let state_before_apply = std::fs::read(&state_path).unwrap();
    std::fs::write(&local_path, edited_bytes).unwrap();

    let error = apply_prepared_pull(prepared, ExecutionMode::Apply).unwrap_err();

    assert!(
        error
            .downcast_ref::<Exit>()
            .is_some_and(|exit| matches!(exit, Exit::Conflict(_))),
        "got: {error:#}"
    );
    assert_eq!(std::fs::read(&local_path).unwrap(), edited_bytes);
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before_apply);
}

#[test]
#[ignore]
fn prepared_force_pull_requires_a_remote_file() {
    let fixture = start_ftp();
    let local = tempfile::tempdir().unwrap();
    let config = write_config(local.path(), &fixture);
    let rel = "prepared/missing-remote.txt";
    let absent_rel = "prepared/also-missing-remotely.txt";
    let local_path = local.path().join(rel);
    let absent_local_path = local.path().join(absent_rel);
    let state_path = local.path().join(".ferry/state.json");
    let local_bytes = b"existing local bytes must remain\n\0";
    std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
    std::fs::write(&local_path, local_bytes).unwrap();
    seed_state(local.path(), rel, b"previously synchronized bytes\n");
    let state_before = std::fs::read(&state_path).unwrap();

    let error = prepare_force_pull_one(&config, rel).unwrap_err();

    assert!(format!("{error:#}").contains(rel));
    assert_eq!(std::fs::read(&local_path).unwrap(), local_bytes);
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);

    let absent_error = prepare_force_pull_one(&config, absent_rel).unwrap_err();

    assert!(format!("{absent_error:#}").contains(absent_rel));
    assert!(!absent_local_path.exists());
    assert_eq!(std::fs::read(&state_path).unwrap(), state_before);
}
