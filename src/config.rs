use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, PartialEq)]
pub struct Config {
    pub connection: Connection,
    pub paths: Paths,
    #[serde(default)]
    pub sync: Sync,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Connection {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub password: String,
    #[serde(default = "default_true")]
    pub passive: bool,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Paths {
    #[serde(default = "default_local_root")]
    pub local_root: PathBuf,
    pub remote_root: String,
}

#[derive(Debug, Deserialize, PartialEq, Default)]
pub struct Sync {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
}

fn default_port() -> u16 { 21 }
fn default_true() -> bool { true }
fn default_local_root() -> PathBuf { PathBuf::from(".") }

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        // Errors from this function are tagged with `Exit::Config` so the
        // process exits 3 (config problem) rather than 1 (generic). The path
        // and underlying I/O / parse message are preserved inside the
        // `Exit::Config(...)` payload so the user still sees what went wrong.
        let text = std::fs::read_to_string(path).map_err(|e| {
            crate::error::Exit::Config(format!("reading {}: {e}", path.display()))
        })?;
        let mut cfg: Config = toml::from_str(&text).map_err(|e| {
            crate::error::Exit::Config(format!("parsing {}: {e}", path.display()))
        })?;
        // Resolve local_root relative to the config file's directory. Without
        // this, invoking ferry from a different CWD (as the Claude Code
        // hook does) makes `local_root = "."` point at the wrong tree.
        if cfg.paths.local_root.is_relative() {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    cfg.paths.local_root = parent.join(&cfg.paths.local_root);
                }
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let toml = r#"
            [connection]
            host = "ftp.example.com"
            user = "deploy"
            password = "secret"

            [paths]
            remote_root = "/var/www/site"

            [sync]
            ignore = ["node_modules/", "*.log"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.connection.host, "ftp.example.com");
        assert_eq!(cfg.connection.port, 21);
        assert!(cfg.connection.passive);
        assert_eq!(cfg.paths.remote_root, "/var/www/site");
        assert_eq!(cfg.sync.ignore.len(), 2);
    }

    #[test]
    fn local_root_resolves_relative_to_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join(".ferry.toml");
        std::fs::write(
            &cfg_path,
            r#"
                [connection]
                host = "h"
                user = "u"
                password = "p"
                [paths]
                local_root  = "."
                remote_root = "/"
            "#,
        )
        .unwrap();
        let cfg = Config::load(&cfg_path).unwrap();
        // "." + config-parent should resolve to the config's directory,
        // regardless of what the current cwd is.
        assert_eq!(cfg.paths.local_root, dir.path().join("."));
    }

    #[test]
    fn absolute_local_root_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join(".ferry.toml");
        std::fs::write(
            &cfg_path,
            r#"
                [connection]
                host = "h"
                user = "u"
                password = "p"
                [paths]
                local_root  = "/somewhere/else"
                remote_root = "/"
            "#,
        )
        .unwrap();
        let cfg = Config::load(&cfg_path).unwrap();
        assert_eq!(cfg.paths.local_root, PathBuf::from("/somewhere/else"));
    }

    #[test]
    fn missing_required_field_errors() {
        let toml = r#"
            [connection]
            host = "x"
            user = "u"
            [paths]
            remote_root = "/"
        "#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        assert!(err.to_string().contains("password"));
    }
}
