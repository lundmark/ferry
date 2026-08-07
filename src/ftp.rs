use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::io::Cursor;
use suppaftp::FtpStream;

pub struct Ftp {
    inner: FtpStream,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactFilePresence {
    Present,
    Missing,
}

/// The subset of remote operations the tree walk needs. Exists so `walk_remote`
/// can be exercised against fake servers in unit tests — real FTP servers
/// disagree about how to answer `LIST <file>`, and those disagreements are
/// exactly what the walk has to handle.
pub trait Remote {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<Entry>>;
    /// `SIZE` is defined for files but not directories, so a successful reply
    /// is how we tell the two apart. Mirrors the probe in `rm` and `pull_one`.
    fn file_size(&mut self, path: &str) -> Result<u64>;
    /// An exact, completeness-aware file lookup used only for single-file
    /// transfer safety. The tolerant directory walk deliberately does not use
    /// this method.
    fn exact_file_presence(&mut self, _path: &str) -> Result<ExactFilePresence> {
        anyhow::bail!("exact remote presence lookup unavailable")
    }
}

impl Remote for Ftp {
    fn list_dir(&mut self, dir: &str) -> Result<Vec<Entry>> {
        self.list(dir)
    }
    fn file_size(&mut self, path: &str) -> Result<u64> {
        self.size(path)
    }
    fn exact_file_presence(&mut self, path: &str) -> Result<ExactFilePresence> {
        self.exact_file_presence(path)
    }
}

impl Ftp {
    pub fn connect(host: &str, port: u16, user: &str, pass: &str, passive: bool) -> Result<Self> {
        // Connect + login failures become `Exit::Auth` so the process exits 3
        // (config/auth) rather than 1. The underlying suppaftp message is
        // preserved in the payload so the user still sees the real cause.
        let mut s = FtpStream::connect((host, port))
            .map_err(|e| crate::error::Exit::Auth(format!("ftp connect {host}:{port}: {e}")))?;
        s.login(user, pass)
            .map_err(|e| crate::error::Exit::Auth(format!("ftp login as {user}: {e}")))?;
        s.transfer_type(suppaftp::types::FileType::Binary)
            .context("ftp set binary transfer type")?;
        s.set_mode(if passive {
            suppaftp::Mode::Passive
        } else {
            suppaftp::Mode::Active
        });
        Ok(Self { inner: s })
    }

    pub fn list(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let lines = self
            .inner
            .list(Some(dir))
            .with_context(|| format!("ftp list {dir}"))?;
        Ok(lines
            .iter()
            .filter_map(|line| {
                let f = suppaftp::list::File::from_posix_line(line).ok()?;
                Some(Entry {
                    name: f.name().to_string(),
                    is_dir: f.is_directory(),
                    size: u64::try_from(f.size()).unwrap_or(0),
                    modified: DateTime::<Utc>::from(f.modified()),
                })
            })
            .collect())
    }

    /// Probe exactly one remote pathname through `NLST`. Unlike [`Self::list`]
    /// this is intentionally strict: every returned line must name the
    /// requested path, so malformed, partial, or unrelated replies cannot be
    /// mistaken for authoritative absence.
    pub fn exact_file_presence(&mut self, path: &str) -> Result<ExactFilePresence> {
        let lines = self
            .inner
            .nlst(Some(path))
            .with_context(|| format!("ftp nlst {path}"))?;
        exact_nlst_presence(path, &lines)
    }
}

fn exact_nlst_presence(path: &str, lines: &[String]) -> Result<ExactFilePresence> {
    if lines.is_empty() {
        return Ok(ExactFilePresence::Missing);
    }
    let requested = path.trim_end_matches('/');
    let leaf = requested.rsplit('/').next().unwrap_or(requested);
    for line in lines {
        let name = line.trim().trim_end_matches('/');
        if name.is_empty() || (name != requested && name != leaf) {
            anyhow::bail!("ftp nlst {path}: unexpected exact-listing line {line:?}");
        }
    }
    Ok(ExactFilePresence::Present)
}

impl Ftp {
    pub fn upload_bytes(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let mut r = Cursor::new(data);
        self.inner
            .put_file(remote_path, &mut r)
            .with_context(|| format!("ftp upload {remote_path}"))?;
        Ok(())
    }

    pub fn download(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.download_to(remote_path, &mut buf)?;
        Ok(buf)
    }

    pub fn download_to<W: std::io::Write>(&mut self, remote_path: &str, w: &mut W) -> Result<u64> {
        let mut copied: u64 = 0;
        self.inner
            .retr(remote_path, |r| {
                copied = std::io::copy(r, w).map_err(suppaftp::FtpError::ConnectionError)?;
                Ok(())
            })
            .with_context(|| format!("ftp download {remote_path}"))?;
        Ok(copied)
    }

    pub fn size(&mut self, remote_path: &str) -> Result<u64> {
        let n = self
            .inner
            .size(remote_path)
            .with_context(|| format!("ftp size {remote_path}"))?;
        Ok(n as u64)
    }

    // MDTM is always UTC per RFC 3659 §3.
    pub fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        let naive = self
            .inner
            .mdtm(remote_path)
            .with_context(|| format!("ftp mtime {remote_path}"))?;
        Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.inner
            .rename(from, to)
            .with_context(|| format!("ftp rename {from} -> {to}"))?;
        Ok(())
    }

    pub fn rm(&mut self, path: &str) -> Result<()> {
        self.inner
            .rm(path)
            .with_context(|| format!("ftp rm {path}"))?;
        Ok(())
    }

    /// Remove a remote directory. The server requires it to be empty; callers
    /// that want a recursive delete must remove the contents first and invoke
    /// `rmdir` bottom-up.
    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        self.inner
            .rmdir(path)
            .with_context(|| format!("ftp rmdir {path}"))?;
        Ok(())
    }

    /// Create a remote directory. Returns Ok if the directory was created OR
    /// already exists. Other errors are propagated.
    ///
    /// FTP servers reply 550 for both "already exists" and real failures, and
    /// suppaftp does not distinguish them. To make this idempotent we fall back
    /// to listing the parent directory after a failed mkdir: if the leaf is
    /// present we treat the call as success, otherwise we surface the original
    /// error with context.
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        match self.inner.mkdir(path) {
            Ok(_) => Ok(()),
            Err(e) => {
                let (parent, leaf) = match path.rsplit_once('/') {
                    Some((p, l)) => (if p.is_empty() { "/" } else { p }, l),
                    None => ("/", path),
                };
                if let Ok(lines) = self.inner.list(Some(parent)) {
                    let exists = lines.iter().any(|line| {
                        suppaftp::list::File::from_posix_line(line)
                            .map(|f| f.is_directory() && f.name() == leaf)
                            .unwrap_or(false)
                    });
                    if exists {
                        return Ok(());
                    }
                }
                Err(anyhow::Error::from(e)).with_context(|| format!("ftp mkdir {path}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExactFilePresence, exact_nlst_presence};

    #[test]
    fn exact_nlst_recognizes_a_hidden_requested_name() {
        assert_eq!(
            exact_nlst_presence("/home/test/.hidden", &[".hidden".to_string()]).unwrap(),
            ExactFilePresence::Present
        );
    }

    #[test]
    fn exact_nlst_empty_response_proves_absence() {
        assert_eq!(
            exact_nlst_presence("/home/test/missing", &[]).unwrap(),
            ExactFilePresence::Missing
        );
    }

    #[test]
    fn exact_nlst_rejects_unexpected_raw_lines() {
        let error =
            exact_nlst_presence("/home/test/target", &["not-the-target".to_string()]).unwrap_err();

        assert!(format!("{error:#}").contains("unexpected exact-listing line"));
    }
}
