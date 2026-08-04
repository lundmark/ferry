//! `ferry init` — interactive first-time setup.
//!
//! Two flows depending on `--no-validate`:
//!
//! - `--no-validate` (cheap path): prompt for credentials, write
//!   `.ferry.toml`, update `.gitignore`. No FTP contact.
//! - default (validating path): same as above, plus connect to the remote,
//!   walk local + remote, classify every file into in-sync / local-only /
//!   remote-only / differs, prompt for each differs, and seed
//!   `.ferry/state.json` with the trusted entries.
//!
//! Stdin handling: we use plain `read_line` for the visible prompts. For the
//! password we use `rpassword::read_password()` when stdin is a TTY (so the
//! input is masked in normal use) and fall back to a plain `read_line` when
//! it isn't (so integration tests can pipe answers in via `Stdio::piped()`).

use crate::commands::ExecutionMode;
use crate::commands::pull::download_one;
use crate::commands::push::upload_one;
use crate::commands::walk::{remote_join, walk_local, walk_remote};
use crate::ftp::Ftp;
use crate::hash::{hash_bytes, hash_file};
use crate::ignored::Matcher;
use crate::state::StateFile;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// The ignore defaults written into a freshly-rendered `.ferry.toml`. Kept
/// here as a single source of truth so the validation pass uses the same
/// matcher the rest of the binary will use on subsequent commands.
const DEFAULT_IGNORE: &[&str] = &[".git/", ".ferry/", "node_modules/", "target/", "*.log"];

pub fn run(config_path: &Path, no_validate: bool) -> Result<()> {
    // 1. Refuse if the target config already exists. We don't want to clobber
    //    a working setup; the user can edit or remove it explicitly.
    if config_path.exists() {
        anyhow::bail!(
            "config already exists at {}; remove it first or edit directly",
            config_path.display()
        );
    }

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    // 2. Prompts. Order matches the integration test's input script —
    //    rearranging these will silently break the tests, so be deliberate.
    let host = prompt(&mut stdin, &mut stdout, "host", None)?;
    if host.is_empty() {
        anyhow::bail!("host is required");
    }
    let port_str = prompt(&mut stdin, &mut stdout, "port", Some("21"))?;
    let port: u16 = port_str
        .parse()
        .with_context(|| format!("invalid port: {port_str:?}"))?;
    let user = prompt(&mut stdin, &mut stdout, "user", None)?;
    if user.is_empty() {
        anyhow::bail!("user is required");
    }
    let password = read_password(&mut stdin, &mut stdout)?;
    let remote_root = prompt(&mut stdin, &mut stdout, "remote_root", None)?;
    if remote_root.is_empty() {
        anyhow::bail!("remote_root is required");
    }
    let local_root = prompt(&mut stdin, &mut stdout, "local_root", Some("."))?;

    // 3. Security warning. Stored plaintext is a real footgun — we keep
    //    .ferry.toml out of git via the .gitignore step below, but the
    //    user still needs to know.
    writeln!(
        stdout,
        "\nWARNING: the password is stored in plaintext in {}.\n\
         Make sure this file is in .gitignore and protected appropriately.",
        config_path.display()
    )?;

    // 4. Validation pass (default behavior — skipped with --no-validate).
    //    We do this BEFORE writing the config so a connect/auth failure
    //    aborts cleanly without leaving a half-baked setup behind. The
    //    state file is written by validate_and_resolve itself once it's
    //    happy with what it found.
    if !no_validate {
        validate_and_resolve(
            &host,
            port,
            &user,
            &password,
            Path::new(&local_root),
            &remote_root,
            &mut stdin,
            &mut stdout,
        )?;
    }

    // 5. Compose and write the config. We hand-roll the TOML rather than
    //    going through serde so we control quoting (the `Config` type only
    //    derives `Deserialize` today) and can include a friendly comment
    //    header for users who open the file by hand.
    let cfg_text = render_config(&host, port, &user, &password, &remote_root, &local_root);
    if let Some(parent) = config_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating config parent dir {}", parent.display())
            })?;
        }
    }
    std::fs::write(config_path, &cfg_text)
        .with_context(|| format!("writing {}", config_path.display()))?;

    // 6. .gitignore. We always touch the .gitignore in the current working
    //    directory rather than next to the config file: most users run
    //    `ferry init` from their project root and that's where their git
    //    repo's .gitignore lives. Only add entries that aren't already
    //    present so reruns / hand-edited gitignores stay clean.
    update_gitignore(Path::new(".gitignore"))?;

    writeln!(
        stdout,
        "\nWrote {}.\nRun `ferry status` to verify connection.",
        config_path.display()
    )?;
    Ok(())
}

/// Connect to the remote, hash both sides, classify each path into one of
/// four buckets, prompt the user to resolve `differs`, and seed
/// `<local_root>/.ferry/state.json` with the entries we now trust.
///
/// Stays generic over the I/O handles so tests can drive it with scripted
/// stdin/stdout. The local root is taken as a `Path` here because we need it
/// for filesystem operations rather than re-parsing the user's string.
#[allow(clippy::too_many_arguments)]
fn validate_and_resolve<R: BufRead, W: Write>(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    local_root: &Path,
    remote_root: &str,
    stdin: &mut R,
    stdout: &mut W,
) -> Result<()> {
    // The ignore set mirrors what render_config will write into the new
    // .ferry.toml so the validation pass agrees with every subsequent
    // command on which files are in scope.
    let patterns: Vec<String> = DEFAULT_IGNORE.iter().map(|s| (*s).to_string()).collect();
    let matcher = Matcher::new(&patterns, local_root)
        .context("building ignore matcher for validation")?;

    // Passive mode matches the `Connection::passive` default in config.rs;
    // using anything else here would make the validation pass behave
    // differently from later `status`/`push`/etc. runs.
    let mut ftp = Ftp::connect(host, port, user, password, true)?;

    let mut local_paths: BTreeSet<String> = BTreeSet::new();
    if local_root.is_dir() {
        walk_local(local_root, local_root, &matcher, &mut local_paths)
            .with_context(|| format!("walking local root {}", local_root.display()))?;
    }
    let mut remote_paths: BTreeSet<String> = BTreeSet::new();
    walk_remote(&mut ftp, remote_root, "", &mut remote_paths)
        .with_context(|| format!("walking remote root {remote_root}"))?;

    // Categorize. We deliberately do NOT use state::classify here — there's
    // no prior state, so the meaningful split is just the four buckets the
    // init flow cares about. For each in-sync file we keep the agreed-on
    // hash + remote size so we can seed state without re-downloading later.
    struct InSync {
        rel: String,
        hash: String,
        size: u64,
    }
    let mut in_sync: Vec<InSync> = Vec::new();
    let mut local_only_count: usize = 0;
    let mut remote_only_count: usize = 0;
    let mut differs: Vec<String> = Vec::new();

    let union: BTreeSet<&String> = local_paths.iter().chain(remote_paths.iter()).collect();
    for rel in union {
        let on_local = local_paths.contains(rel);
        let on_remote = remote_paths.contains(rel);
        match (on_local, on_remote) {
            (true, false) => local_only_count += 1,
            (false, true) => remote_only_count += 1,
            (true, true) => {
                let lh = hash_file(&local_root.join(rel))?;
                let remote_path = remote_join(remote_root, rel);
                let rb = ftp
                    .download(&remote_path)
                    .with_context(|| format!("downloading {remote_path} for hash"))?;
                let rh = hash_bytes(&rb);
                if lh == rh {
                    in_sync.push(InSync {
                        rel: rel.clone(),
                        hash: rh,
                        size: rb.len() as u64,
                    });
                } else {
                    differs.push(rel.clone());
                }
            }
            (false, false) => unreachable!("path appeared in union but neither side"),
        }
    }

    writeln!(
        stdout,
        "\nFound {} files in sync, {} local-only, {} remote-only, {} differing.",
        in_sync.len(),
        local_only_count,
        remote_only_count,
        differs.len(),
    )?;

    // Build the state file we're about to seed. In-sync entries go in
    // unconditionally; differs entries go in only if the chosen resolution
    // produces a known-good hash on BOTH sides (p/P). 'k' and 's' leave the
    // file in an indeterminate state and we don't pretend otherwise.
    let mut state = StateFile::default();

    for entry in &in_sync {
        let remote_path = remote_join(remote_root, &entry.rel);
        // We still need a fresh mtime — `mdtm` is a single command, so this
        // is a much cheaper round-trip than re-downloading would have been.
        let mtime = ftp
            .mtime(&remote_path)
            .with_context(|| format!("fetching mtime for {remote_path}"))?;
        state.files.insert(
            entry.rel.clone(),
            crate::state::FileRecord {
                sha256: entry.hash.clone(),
                size: entry.size,
                remote_mtime: mtime,
                last_synced: chrono::Utc::now(),
            },
        );
    }

    // Resolve each differ interactively. The default on empty/EOF is 'k'
    // (keep unsynced) so accidentally running init through a non-interactive
    // shell can never silently mutate either side.
    for rel in &differs {
        writeln!(stdout, "\ndiffers: {rel}")?;
        write!(stdout, "[k]eep unsynced  [p]ush local  [P]ull remote  [s]kip: ")?;
        stdout.flush()?;
        let choice = read_single_char(stdin).unwrap_or('k');

        match choice {
            'p' => {
                let local_path = local_root.join(rel);
                let bytes = std::fs::read(&local_path)
                    .with_context(|| format!("reading local {}", local_path.display()))?;
                let new_hash = hash_bytes(&bytes);
                let remote_path = remote_join(remote_root, rel);
                upload_one(
                    &mut ftp,
                    &mut state,
                    rel,
                    &remote_path,
                    &bytes,
                    &new_hash,
                    ExecutionMode::Apply,
                )?;
                writeln!(stdout, "pushed {rel}")?;
            }
            'P' => {
                let remote_path = remote_join(remote_root, rel);
                let bytes = ftp
                    .download(&remote_path)
                    .with_context(|| format!("downloading {remote_path}"))?;
                let new_hash = hash_bytes(&bytes);
                let local_path = local_root.join(rel);
                download_one(
                    &mut ftp,
                    &mut state,
                    &local_path,
                    rel,
                    &remote_path,
                    &bytes,
                    &new_hash,
                )?;
                writeln!(stdout, "pulled {rel}")?;
            }
            's' => {
                writeln!(stdout, "skipped {rel}")?;
            }
            _ => {
                // 'k' or any unrecognized input — leave both sides alone and
                // do not record a state entry. Subsequent `status` will
                // classify this as Untracked.
                writeln!(stdout, "keeping {rel} unsynced")?;
            }
        }
    }

    // Persist the seeded state. We skip writing if there's nothing to
    // record AND the file doesn't already exist — no point creating an
    // empty file just to demonstrate the directory exists. (If the user
    // had a prior state.json we'd overwrite it; but init refuses to run
    // when the config exists, so reaching here implies a fresh setup.)
    if !state.files.is_empty() {
        let state_path: PathBuf = local_root.join(crate::names::STATE_DIR).join("state.json");
        state.save(&state_path)
            .with_context(|| format!("writing state file {}", state_path.display()))?;
    }

    Ok(())
}

/// Read one character from `stdin`, ignoring leading whitespace, then drain
/// the rest of the line so the next prompt starts cleanly. Returns `None`
/// on EOF before any non-whitespace character.
fn read_single_char<R: BufRead>(stdin: &mut R) -> Option<char> {
    let mut line = String::new();
    let n = stdin.read_line(&mut line).ok()?;
    if n == 0 {
        return None;
    }
    line.chars().find(|c| !c.is_whitespace())
}

fn prompt<R: BufRead, W: Write>(
    stdin: &mut R,
    stdout: &mut W,
    label: &str,
    default: Option<&str>,
) -> Result<String> {
    match default {
        Some(d) => write!(stdout, "{label} [{d}]: ")?,
        None => write!(stdout, "{label}: ")?,
    }
    stdout.flush()?;
    let mut line = String::new();
    let n = stdin.read_line(&mut line)?;
    if n == 0 {
        // EOF before any input — treat as empty (default will apply).
    }
    let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
    if trimmed.is_empty() {
        if let Some(d) = default {
            return Ok(d.to_string());
        }
    }
    Ok(trimmed)
}

/// Read a password, masking when stdin is a TTY and falling back to a plain
/// line read when it isn't. The fallback is what lets integration tests
/// drive the prompt with `Stdio::piped()`.
fn read_password<R: BufRead, W: Write>(stdin: &mut R, stdout: &mut W) -> Result<String> {
    write!(stdout, "password: ")?;
    stdout.flush()?;
    if std::io::stdin().is_terminal() {
        // rpassword opens /dev/tty under the hood and echoes nothing.
        let pw = rpassword::read_password().context("reading password")?;
        // rpassword strips the trailing newline already; print one so the
        // next prompt doesn't sit on the same line as the masked input.
        writeln!(stdout)?;
        Ok(pw)
    } else {
        let mut line = String::new();
        stdin.read_line(&mut line)?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
}

fn render_config(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    remote_root: &str,
    local_root: &str,
) -> String {
    // The ignore defaults here mirror DEFAULT_IGNORE so the validation pass
    // and the written config agree on what's in scope.
    let ignore_list: String = DEFAULT_IGNORE
        .iter()
        .map(|s| toml_string(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# ferry configuration — generated by `ferry init`.\n\
         # The password is stored in plaintext; keep this file out of git.\n\
         \n\
         [connection]\n\
         host = {host}\n\
         port = {port}\n\
         user = {user}\n\
         password = {password}\n\
         \n\
         [paths]\n\
         local_root = {local_root}\n\
         remote_root = {remote_root}\n\
         \n\
         [sync]\n\
         ignore = [{ignore_list}]\n",
        host = toml_string(host),
        port = port,
        user = toml_string(user),
        password = toml_string(password),
        remote_root = toml_string(remote_root),
        local_root = toml_string(local_root),
        ignore_list = ignore_list,
    )
}

/// Minimal TOML string escaping for the handful of fields we write. We only
/// need to handle backslashes and double quotes; everything else passes
/// through (TOML's basic-string accepts ordinary printable characters).
/// Non-printable bytes in a password would be unusual but we still escape
/// the obvious troublemakers so we don't emit invalid TOML.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Append the config file and state dir to `path`, creating the file if
/// missing. Skip entries that are already present (matching the whole line
/// after trimming) so repeated inits don't keep growing the file.
fn update_gitignore(path: &Path) -> Result<()> {
    let state_dir_entry = format!("{}/", crate::names::STATE_DIR);
    let entries: [&str; 2] = [crate::names::CONFIG_FILE, &state_dir_entry];
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("reading {}", path.display())));
        }
    };
    let have: std::collections::HashSet<&str> =
        existing.lines().map(|l| l.trim()).collect();

    let mut to_add: Vec<&str> = Vec::new();
    for entry in entries {
        if !have.contains(entry) {
            to_add.push(entry);
        }
    }
    if to_add.is_empty() {
        return Ok(());
    }

    let mut new_text = existing.clone();
    // Make sure we start the new entries on their own line. If the file is
    // non-empty and doesn't end in a newline, add one before appending.
    if !new_text.is_empty() && !new_text.ends_with('\n') {
        new_text.push('\n');
    }
    for entry in to_add {
        new_text.push_str(entry);
        new_text.push('\n');
    }
    std::fs::write(path, new_text)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_string_escapes_quotes_and_backslashes() {
        assert_eq!(toml_string("a"), "\"a\"");
        assert_eq!(toml_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(toml_string("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn render_config_round_trips_through_loader() {
        // The whole point of this command is to write a file the rest of
        // the binary can read back. Make sure the loader actually accepts
        // what we render.
        let text = render_config("ftp.example.com", 21, "deploy", "s3cr3t", "/var/www/site", ".");
        let parsed: crate::config::Config =
            toml::from_str(&text).expect("render_config produced unparseable TOML");
        assert_eq!(parsed.connection.host, "ftp.example.com");
        assert_eq!(parsed.connection.port, 21);
        assert_eq!(parsed.connection.user, "deploy");
        assert_eq!(parsed.connection.password, "s3cr3t");
        assert_eq!(parsed.paths.remote_root, "/var/www/site");
        assert_eq!(parsed.paths.local_root, std::path::PathBuf::from("."));
        assert_eq!(parsed.sync.ignore.len(), 5);
    }

    #[test]
    fn update_gitignore_creates_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".gitignore");
        update_gitignore(&gi).unwrap();
        let text = std::fs::read_to_string(&gi).unwrap();
        assert!(text.contains(".ferry.toml"));
        assert!(text.contains(".ferry/"));
    }

    #[test]
    fn update_gitignore_skips_existing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".gitignore");
        std::fs::write(&gi, "target/\n.ferry.toml\n").unwrap();
        update_gitignore(&gi).unwrap();
        let text = std::fs::read_to_string(&gi).unwrap();
        // .ferry.toml shouldn't be duplicated; .ferry/ should be added once.
        assert_eq!(text.matches(".ferry.toml").count(), 1);
        assert_eq!(text.matches(".ferry/").count(), 1);
    }

    #[test]
    fn update_gitignore_adds_trailing_newline_if_needed() {
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".gitignore");
        std::fs::write(&gi, "target/").unwrap(); // no trailing newline
        update_gitignore(&gi).unwrap();
        let text = std::fs::read_to_string(&gi).unwrap();
        // The new entries must be on their own lines, not concatenated.
        assert!(text.contains("target/\n.ferry.toml"));
    }
}
