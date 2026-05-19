use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::io::Cursor;
use suppaftp::FtpStream;

pub struct Ftp {
    inner: FtpStream,
}

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

impl Ftp {
    pub fn connect(host: &str, port: u16, user: &str, pass: &str, passive: bool) -> Result<Self> {
        let mut s = FtpStream::connect((host, port)).context("ftp connect")?;
        s.login(user, pass).context("ftp login")?;
        s.set_mode(if passive { suppaftp::Mode::Passive } else { suppaftp::Mode::Active });
        Ok(Self { inner: s })
    }

    pub fn list(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let lines = self.inner.list(Some(dir))?;
        Ok(lines.iter().filter_map(|line| parse_list_line(line)).collect())
    }

    pub fn upload_bytes(&mut self, remote_path: &str, data: &[u8]) -> Result<()> {
        let mut r = Cursor::new(data);
        self.inner.put_file(remote_path, &mut r)?;
        Ok(())
    }

    pub fn download(&mut self, remote_path: &str) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.inner.retr(remote_path, |r| {
            std::io::copy(r, &mut buf).map(|_| ()).map_err(suppaftp::FtpError::ConnectionError)
        })?;
        Ok(buf)
    }

    pub fn size(&mut self, remote_path: &str) -> Result<u64> {
        Ok(self.inner.size(remote_path)? as u64)
    }

    pub fn mtime(&mut self, remote_path: &str) -> Result<DateTime<Utc>> {
        let naive = self.inner.mdtm(remote_path)?;
        Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.inner.rename(from, to)?;
        Ok(())
    }

    pub fn rm(&mut self, path: &str) -> Result<()> {
        self.inner.rm(path)?;
        Ok(())
    }
}

fn parse_list_line(line: &str) -> Option<Entry> {
    // "-rw-r--r--   1 ftp ftp     6 May 17 08:00 hello.txt"
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }
    let is_dir = parts[0].starts_with('d');
    let size = parts[4].parse().unwrap_or(0);
    let name = parts[8..].join(" ");
    Some(Entry { name, is_dir, size })
}
