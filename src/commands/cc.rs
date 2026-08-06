use std::path::Path;

use anyhow::{anyhow, Result};

use crate::commands::walk;
use crate::config::Config;
use crate::udp::{CheckResult, CompileClient};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCheckStatus {
    Passed,
    Failed,
    TransportError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheckResult {
    pub path: String,
    pub status: FileCheckStatus,
    pub diagnostics: String,
}

trait CompileTransport {
    fn check(&self, user: &str, password: &str, path: &str) -> Result<CheckResult>;
}

impl CompileTransport for CompileClient {
    fn check(&self, user: &str, password: &str, path: &str) -> Result<CheckResult> {
        CompileClient::check(self, user, password, path)
    }
}

/// Check-compile files and return all per-file outcomes without printing.
pub fn check_files(config_path: &Path, paths: &[String]) -> Result<Vec<FileCheckResult>> {
    let cfg = Config::load(config_path)?;
    let paths = checked_paths(&cfg, paths)?;
    let client = CompileClient::new(&cfg.connection.host, cfg.connection.udp_port)?;

    check_resolved_with(&cfg, &paths, &client)
}

/// `ferry cc <paths...>` — print per-file check results and return an error
/// if any compilation or transport check failed.
pub fn run(config_path: &Path, paths: &[String]) -> Result<()> {
    let results = check_files(config_path, paths)?;
    let mut any_fail = false;

    for result in results {
        match result.status {
            FileCheckStatus::Passed => {
                println!("{}: OK", result.path);
                print_diag(&result.diagnostics);
            }
            FileCheckStatus::Failed => {
                any_fail = true;
                println!("{}: FAIL", result.path);
                print_diag(&result.diagnostics);
            }
            FileCheckStatus::TransportError(error) => {
                any_fail = true;
                eprintln!("{}: error: {error}", result.path);
            }
        }
    }

    if any_fail {
        Err(anyhow!("one or more compile checks failed"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn check_with<T: CompileTransport + ?Sized>(
    cfg: &Config,
    paths: &[String],
    transport: &T,
) -> Result<Vec<FileCheckResult>> {
    let paths = checked_paths(cfg, paths)?;

    check_resolved_with(cfg, &paths, transport)
}

fn checked_paths(cfg: &Config, paths: &[String]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|path| walk::safe_arg(&cfg.paths.local_root, path))
        .collect()
}

fn check_resolved_with<T: CompileTransport + ?Sized>(
    cfg: &Config,
    paths: &[String],
    transport: &T,
) -> Result<Vec<FileCheckResult>> {
    paths
        .iter()
        .map(|path| {
            let remote = walk::remote_join(&cfg.paths.remote_root, path);
            match transport.check(&cfg.connection.user, &cfg.connection.password, &remote) {
                Ok(result) => Ok(FileCheckResult {
                    path: path.clone(),
                    status: if result.ok {
                        FileCheckStatus::Passed
                    } else {
                        FileCheckStatus::Failed
                    },
                    diagnostics: result.diagnostics,
                }),
                Err(error) => Ok(FileCheckResult {
                    path: path.clone(),
                    status: FileCheckStatus::TransportError(format!("{error:#}")),
                    diagnostics: String::new(),
                }),
            }
        })
        .collect()
}

fn print_diag(diagnostics: &str) {
    for line in diagnostics.lines() {
        if !line.is_empty() {
            println!("  {line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use anyhow::{anyhow, Result};

    use super::*;
    use crate::config::{Connection, Editor, Paths, Sync};
    use crate::udp::CheckResult;

    struct FakeTransport {
        responses: RefCell<VecDeque<Result<CheckResult>>>,
        paths: RefCell<Vec<String>>,
    }

    impl FakeTransport {
        fn new(responses: impl IntoIterator<Item = Result<CheckResult>>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                paths: RefCell::new(Vec::new()),
            }
        }
    }

    impl CompileTransport for FakeTransport {
        fn check(&self, _user: &str, _password: &str, path: &str) -> Result<CheckResult> {
            self.paths.borrow_mut().push(path.to_string());
            self.responses.borrow_mut().pop_front().unwrap()
        }
    }

    fn config(local_root: std::path::PathBuf) -> Config {
        Config {
            connection: Connection {
                host: "example.test".to_string(),
                port: 21,
                udp_port: 3203,
                user: "user".to_string(),
                password: "password".to_string(),
                passive: true,
            },
            paths: Paths {
                local_root,
                remote_root: "/mudlib".to_string(),
            },
            sync: Sync::default(),
            editor: Editor::default(),
        }
    }

    #[test]
    fn check_with_collects_ordered_results_and_continues_after_transport_error() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path().to_path_buf());
        let transport = FakeTransport::new([
            Ok(CheckResult {
                ok: true,
                diagnostics: "note: clean\n".to_string(),
            }),
            Ok(CheckResult {
                ok: false,
                diagnostics: "two.c:4: error: broken\n".to_string(),
            }),
            Err(anyhow!("UDP timed out")),
        ]);

        let results = check_with(
            &cfg,
            &[
                "one.c".to_string(),
                "two.c".to_string(),
                "three.c".to_string(),
            ],
            &transport,
        )
        .unwrap();

        assert_eq!(
            results,
            vec![
                FileCheckResult {
                    path: "one.c".to_string(),
                    status: FileCheckStatus::Passed,
                    diagnostics: "note: clean\n".to_string(),
                },
                FileCheckResult {
                    path: "two.c".to_string(),
                    status: FileCheckStatus::Failed,
                    diagnostics: "two.c:4: error: broken\n".to_string(),
                },
                FileCheckResult {
                    path: "three.c".to_string(),
                    status: FileCheckStatus::TransportError("UDP timed out".to_string()),
                    diagnostics: String::new(),
                },
            ]
        );
        assert_eq!(
            *transport.paths.borrow(),
            vec![
                "/mudlib/one.c".to_string(),
                "/mudlib/two.c".to_string(),
                "/mudlib/three.c".to_string(),
            ]
        );
    }

    #[test]
    fn check_with_normalizes_safe_absolute_paths_and_validates_all_inputs_before_transport() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path().to_path_buf());
        let safe_absolute = root.path().join("nested/safe.c");
        std::fs::create_dir(root.path().join("nested")).unwrap();
        let transport = FakeTransport::new([Ok(CheckResult {
            ok: true,
            diagnostics: String::new(),
        })]);

        let results = check_with(&cfg, &[safe_absolute.display().to_string()], &transport).unwrap();
        assert_eq!(results[0].path, "nested/safe.c");

        let no_calls = FakeTransport::new(std::iter::empty());
        let outside = root.path().parent().unwrap().join("outside.c");
        let error = check_with(
            &cfg,
            &["valid.c".to_string(), outside.display().to_string()],
            &no_calls,
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
        assert!(no_calls.paths.borrow().is_empty());
    }
}
