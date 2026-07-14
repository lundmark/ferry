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
        s.set_mode(if passive { suppaftp::Mode::Passive } else { suppaftp::Mode::Active });
        Ok(Self { inner: s })
    }

    pub fn list(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let lines = self.inner.list(Some(dir))
            .with_context(|| format!("ftp list {dir}"))?;
        Ok(lines.iter().filter_map(|line| {
            let f = suppaftp::list::File::from_posix_line(line).ok()?;
            Some(Entry {
                name: f.name().to_string(),
                is_dir: f.is_directory(),
                size: u64::try_from(f.size()).unwrap_or(0),
                modified: DateTime::<Utc>::from(f.modified()),
            })
        }).collect())
    }

    pub fn upload_bytes(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let mut r = Cursor::new(data);
        self.inner.put_file(remote_path, &mut r)
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
        self.inner.retr(remote_path, |r| {
            copied = std::io::copy(r, w)
                .map_err(suppaftp::FtpError::ConnectionError)?;
            Ok(())
        }).with_context(|| format!("ftp download {remote_path}"))?;
        Ok(copied)
    }

    pub fn size(&mut self, remote_path: &str) -> Result<u64> {
        let n = self.inner.size(remote_path)
            .with_context(|| format!("ftp size {remote_path}"))?;
        Ok(n as u64)
    }

    // MDTM is always UTC per RFC 3659 §3.
    pub fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        let naive = self.inner.mdtm(remote_path)
            .with_context(|| format!("ftp mtime {remote_path}"))?;
        Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.inner.rename(from, to)
            .with_context(|| format!("ftp rename {from} -> {to}"))?;
        Ok(())
    }

    pub fn rm(&mut self, path: &str) -> Result<()> {
        self.inner.rm(path)
            .with_context(|| format!("ftp rm {path}"))?;
        Ok(())
    }

    /// Remove a remote directory. The server requires it to be empty; callers
    /// that want a recursive delete must remove the contents first and invoke
    /// `rmdir` bottom-up.
    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        self.inner.rmdir(path)
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
