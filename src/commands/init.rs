//! `zed-ftp init` — interactive first-time setup.
//!
//! Task 14 only implements the basic / `--no-validate` flow: prompt for
//! credentials, write `.zed-ftp.toml`, and append the `.zed-ftp.toml` and
//! `.zed-ftp/` entries to `.gitignore`. We do **not** connect to FTP here —
//! Task 15 will add the existing-files validation pass (walk + classify +
//! resolve). For now `--no-validate` is implied even when the flag is absent.
//!
//! Stdin handling: we use plain `read_line` for the visible prompts. For the
//! password we use `rpassword::read_password()` when stdin is a TTY (so the
//! input is masked in normal use) and fall back to a plain `read_line` when
//! it isn't (so integration tests can pipe answers in via `Stdio::piped()`).

use anyhow::{Context, Result};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

pub fn run(config_path: &Path, _no_validate: bool) -> Result<()> {
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
    //    .zed-ftp.toml out of git via the .gitignore step below, but the
    //    user still needs to know.
    writeln!(
        stdout,
        "\nWARNING: the password is stored in plaintext in {}.\n\
         Make sure this file is in .gitignore and protected appropriately.",
        config_path.display()
    )?;

    // 4. Compose and write the config. We hand-roll the TOML rather than
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

    // 5. .gitignore. We always touch the .gitignore in the current working
    //    directory rather than next to the config file: most users run
    //    `zed-ftp init` from their project root and that's where their git
    //    repo's .gitignore lives. Only add entries that aren't already
    //    present so reruns / hand-edited gitignores stay clean.
    update_gitignore(Path::new(".gitignore"))?;

    // 6. Task 15 will replace this with a real connect + validate. For now,
    //    point the user at `status` so they can confirm the credentials work.
    writeln!(
        stdout,
        "\nWrote {}.\nRun `zed-ftp status` to verify connection.",
        config_path.display()
    )?;
    Ok(())
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
    // The ignore defaults here match the spec in Task 14. They're a
    // reasonable baseline for most projects; users can edit later.
    format!(
        "# zed-ftp configuration — generated by `zed-ftp init`.\n\
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
         ignore = [\".git/\", \".zed-ftp/\", \"node_modules/\", \"target/\", \"*.log\"]\n",
        host = toml_string(host),
        port = port,
        user = toml_string(user),
        password = toml_string(password),
        remote_root = toml_string(remote_root),
        local_root = toml_string(local_root),
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

/// Append `.zed-ftp.toml` and `.zed-ftp/` to `path`, creating the file if
/// missing. Skip entries that are already present (matching the whole line
/// after trimming) so repeated inits don't keep growing the file.
fn update_gitignore(path: &Path) -> Result<()> {
    const ENTRIES: &[&str] = &[".zed-ftp.toml", ".zed-ftp/"];
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
    for entry in ENTRIES {
        if !have.contains(*entry) {
            to_add.push(*entry);
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
        assert!(text.contains(".zed-ftp.toml"));
        assert!(text.contains(".zed-ftp/"));
    }

    #[test]
    fn update_gitignore_skips_existing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".gitignore");
        std::fs::write(&gi, "target/\n.zed-ftp.toml\n").unwrap();
        update_gitignore(&gi).unwrap();
        let text = std::fs::read_to_string(&gi).unwrap();
        // .zed-ftp.toml shouldn't be duplicated; .zed-ftp/ should be added once.
        assert_eq!(text.matches(".zed-ftp.toml").count(), 1);
        assert_eq!(text.matches(".zed-ftp/").count(), 1);
    }

    #[test]
    fn update_gitignore_adds_trailing_newline_if_needed() {
        let dir = tempfile::tempdir().unwrap();
        let gi = dir.path().join(".gitignore");
        std::fs::write(&gi, "target/").unwrap(); // no trailing newline
        update_gitignore(&gi).unwrap();
        let text = std::fs::read_to_string(&gi).unwrap();
        // The new entries must be on their own lines, not concatenated.
        assert!(text.contains("target/\n.zed-ftp.toml"));
    }
}
