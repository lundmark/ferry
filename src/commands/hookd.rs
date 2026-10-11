//! `ferry hookd` — a per-project helper that keeps one FTP session logged in
//! for `ferry hook`.
//!
//! The editor hook runs for every file an agent reads or edits, and each run
//! used to open a fresh connection: about 0.7s of connect and login before the
//! one-file check even started. The helper holds the session instead. `ferry
//! hook` sends it one request per file over a Unix socket in the project's
//! state directory and prints the reply; when no helper answers, the hook
//! starts one in the background and does that request itself the old way, so
//! the hook never waits on the helper and never gets worse than before.
//!
//! The helper serves one request at a time, sends a NOOP while idle so the
//! server keeps the session, reconnects when it has to, and exits after
//! `IDLE_EXIT` without requests. Every request starts with the session's
//! listing caches cleared: a listing from minutes ago must not vouch for a
//! file now. Unix only; elsewhere the hook keeps connecting per file.

use crate::config::Config;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Wire version of the request/response lines. A helper left running from an
/// older ferry answers a newer hook with an error, and the hook falls back.
const PROTOCOL: u32 = 2;

/// The helper's socket for the project rooted at `local_root`.
pub fn socket_path(local_root: &Path) -> PathBuf {
    local_root.join(crate::names::STATE_DIR).join("hookd.sock")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    v: u32,
    rel: String,
    force: bool,
    /// Skip the pull if the file was synced within this many seconds.
    cooldown: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Response {
    ok: bool,
    /// What the hook should print (empty for nothing to report).
    msg: String,
    /// The file was synced within the cooldown, so nothing was checked.
    #[serde(default)]
    cooled: bool,
}

/// What a helper said about one request.
pub enum Answer {
    /// Synced within the cooldown; nothing was checked.
    Cooled,
    /// Checked (and pulled if needed). The text to print, empty for nothing.
    Done(String),
}

#[cfg(unix)]
mod imp {
    use super::*;
    use crate::commands::file_transfer::TransferStatus;
    use crate::commands::{ExecutionMode, state_path_for};
    use crate::ftp::Ftp;
    use crate::state::StateFile;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::{Duration, Instant};

    /// Exit after this long without a request.
    const IDLE_EXIT: Duration = Duration::from_secs(15 * 60);
    /// NOOP the idle session this often, well inside the server's timeout.
    const KEEPALIVE: Duration = Duration::from_secs(60);
    /// How long a hook waits for the helper's answer before falling back.
    const CLIENT_WAIT: Duration = Duration::from_secs(60);

    /// Ask a running helper to pull `rel` unless it was synced within
    /// `cooldown` seconds. `None` means no helper answered (none running, a
    /// stale socket, a protocol mismatch, a timeout): the caller should do the
    /// work itself.
    pub fn request(local_root: &Path, rel: &str, force: bool, cooldown: i64) -> Option<Answer> {
        let mut stream = UnixStream::connect(socket_path(local_root)).ok()?;
        stream.set_read_timeout(Some(CLIENT_WAIT)).ok()?;
        stream.set_write_timeout(Some(CLIENT_WAIT)).ok()?;
        let line = serde_json::to_string(&Request {
            v: PROTOCOL,
            rel: rel.to_string(),
            force,
            cooldown,
        })
        .ok()?;
        stream.write_all(format!("{line}\n").as_bytes()).ok()?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).ok()?;
        let response: Response = serde_json::from_str(reply.trim()).ok()?;
        if !response.ok {
            return None;
        }
        Some(if response.cooled {
            Answer::Cooled
        } else {
            Answer::Done(response.msg)
        })
    }

    /// Start a helper for `config_path` in the background, detached from the
    /// hook's process group so it outlives the hook. Best effort: if it cannot
    /// start, the hook simply keeps connecting per file.
    pub fn spawn(config_path: &Path) {
        use std::os::unix::process::CommandExt;
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let _ = std::process::Command::new(exe)
            .arg("--config")
            .arg(config_path)
            .arg("hookd")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn();
    }

    /// The helper itself. Returns quietly if another helper already owns the
    /// socket.
    pub fn run(config_path: &Path) -> Result<()> {
        let cfg = Config::load(config_path)?;
        let socket = socket_path(&cfg.paths.local_root);
        let Some(listener) = bind(&socket)? else {
            return Ok(());
        };
        let result = serve(&cfg, config_path, &listener);
        let _ = std::fs::remove_file(&socket);
        result
    }

    /// Bind the socket, owner-only. `None` when a live helper already has it;
    /// a socket file nobody answers on is left over from a crash and removed.
    fn bind(socket: &Path) -> Result<Option<UnixListener>> {
        use std::os::unix::fs::PermissionsExt;
        if socket.exists() {
            if UnixStream::connect(socket).is_ok() {
                return Ok(None);
            }
            std::fs::remove_file(socket)
                .with_context(|| format!("removing stale {}", socket.display()))?;
        }
        if let Some(dir) = socket.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let listener = match UnixListener::bind(socket) {
            Ok(listener) => listener,
            // Another helper won the race between the check and the bind.
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("binding {}", socket.display()));
            }
        };
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", socket.display()))?;
        listener
            .set_nonblocking(true)
            .context("making the hook socket non-blocking")?;
        Ok(Some(listener))
    }

    /// Accept and answer requests until the helper has been idle for
    /// `IDLE_EXIT`, keeping the session alive in between.
    fn serve(cfg: &Config, config_path: &Path, listener: &UnixListener) -> Result<()> {
        let mut session: Option<Ftp> = None;
        let mut state = StateCache::new(state_path_for(&cfg.paths.local_root, ExecutionMode::Apply));
        let mut last_request = Instant::now();
        let mut last_keepalive = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    answer(cfg, config_path, &mut session, &mut state, stream);
                    last_request = Instant::now();
                    last_keepalive = Instant::now();
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e).context("accepting a hook request"),
            }
            if last_request.elapsed() >= IDLE_EXIT {
                return Ok(());
            }
            if last_keepalive.elapsed() >= KEEPALIVE {
                last_keepalive = Instant::now();
                // A session that fails its NOOP is dropped; the next request
                // logs in again.
                if let Some(ftp) = session.as_mut()
                    && ftp.noop().is_err()
                {
                    session = None;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Read one request, pull, and write the response. A request that cannot
    /// be read or answered is dropped; the hook falls back on its own.
    fn answer(
        cfg: &Config,
        config_path: &Path,
        session: &mut Option<Ftp>,
        state: &mut StateCache,
        stream: UnixStream,
    ) {
        if stream.set_nonblocking(false).is_err()
            || stream.set_read_timeout(Some(Duration::from_secs(5))).is_err()
        {
            return;
        }
        let mut line = String::new();
        let mut reader = BufReader::new(&stream);
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let response = match serde_json::from_str::<Request>(line.trim()) {
            Ok(request) if request.v == PROTOCOL => match state.current() {
                Ok(current) if cooled(current, &request) => Response {
                    ok: true,
                    msg: String::new(),
                    cooled: true,
                },
                Ok(current) => Response {
                    ok: true,
                    msg: pull(cfg, config_path, session, current, &request),
                    cooled: false,
                },
                // The state would not load: let the hook do this one itself.
                Err(_) => Response {
                    ok: false,
                    msg: "state unavailable".into(),
                    cooled: false,
                },
            },
            _ => Response {
                ok: false,
                msg: "unsupported request".into(),
                cooled: false,
            },
        };
        if let Ok(text) = serde_json::to_string(&response) {
            let mut stream = &stream;
            let _ = stream.write_all(format!("{text}\n").as_bytes());
        }
    }

    /// Pull one file on the kept session, logging in when there is none and
    /// trying once more on a fresh login if the session turns out to be dead
    /// (the server dropped it since the last keepalive). Returns what the hook
    /// should print, in the same words the hook itself would use.
    fn pull(
        cfg: &Config,
        config_path: &Path,
        session: &mut Option<Ftp>,
        state: &mut StateFile,
        request: &Request,
    ) -> String {
        let mut last_error = None;
        for _attempt in 0..2 {
            if session.is_none() {
                match connect(cfg) {
                    Ok(ftp) => *session = Some(ftp),
                    Err(e) => {
                        last_error = Some(e);
                        continue;
                    }
                }
            }
            let ftp = session.as_mut().expect("session set above");
            ftp.forget_session_caches();
            match crate::commands::pull::pull_one_on(
                ftp,
                state,
                config_path,
                &request.rel,
                request.force,
                ExecutionMode::Apply,
            ) {
                Ok(outcome) if outcome.status == TransferStatus::Transferred => {
                    return format!("pulled {}", outcome.path);
                }
                Ok(_) => return String::new(),
                // A live session means the error is about the file itself
                // (missing on both sides, say): report it as the hook would.
                Err(e) if ftp.noop().is_ok() => {
                    return format!("pull {} failed: {e:#}", request.rel);
                }
                // A dead one: log in again and retry.
                Err(e) => {
                    *session = None;
                    last_error = Some(e);
                }
            }
        }
        match last_error {
            Some(e) => format!("pull {} failed: {e:#}", request.rel),
            None => String::new(),
        }
    }

    /// Whether `request.rel` was synced within the request's cooldown: the
    /// same test the hook makes when it works alone.
    fn cooled(state: &StateFile, request: &Request) -> bool {
        state.files.get(&request.rel).is_some_and(|record| {
            let elapsed = chrono::Utc::now().signed_duration_since(record.last_synced);
            elapsed.num_seconds() >= 0 && elapsed.num_seconds() < request.cooldown
        })
    }

    /// The project's state, kept in memory between requests. The file is
    /// large (it records every synced file), so it is read again only when
    /// its modification time or size shows another process changed it.
    struct StateCache {
        path: PathBuf,
        stamp: Option<(std::time::SystemTime, u64)>,
        state: Option<StateFile>,
    }

    impl StateCache {
        fn new(path: PathBuf) -> Self {
            Self {
                path,
                stamp: None,
                state: None,
            }
        }

        fn current(&mut self) -> Result<&mut StateFile> {
            let stamp = std::fs::metadata(&self.path)
                .ok()
                .and_then(|m| Some((m.modified().ok()?, m.len())));
            if self.state.is_none() || stamp.is_none() || stamp != self.stamp {
                self.state = Some(StateFile::load_or_default(&self.path)?);
                self.stamp = stamp;
            }
            Ok(self.state.as_mut().expect("state loaded above"))
        }
    }

    fn connect(cfg: &Config) -> Result<Ftp> {
        Ftp::connect(
            &cfg.connection.host,
            cfg.connection.port,
            &cfg.connection.user,
            &cfg.connection.password,
            cfg.connection.passive,
        )
    }
}

#[cfg(unix)]
pub use imp::{request, run, spawn};

#[cfg(not(unix))]
pub fn request(_local_root: &Path, _rel: &str, _force: bool) -> Option<String> {
    None
}

#[cfg(not(unix))]
pub fn spawn(_config_path: &Path) {}

#[cfg(not(unix))]
pub fn run(_config_path: &Path) -> Result<()> {
    anyhow::bail!("ferry hookd needs Unix sockets")
}
