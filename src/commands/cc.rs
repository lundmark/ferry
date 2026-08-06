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
    check_with_factory(&cfg, paths, || {
        CompileClient::new(&cfg.connection.host, cfg.connection.udp_port)
    })
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

fn check_with_factory<T, F>(
    cfg: &Config,
    paths: &[String],
    make_transport: F,
) -> Result<Vec<FileCheckResult>>
where
    T: CompileTransport,
    F: FnOnce() -> Result<T>,
{
    let paths = checked_paths(cfg, paths)?;
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let transport = make_transport()?;

    Ok(check_resolved_with(cfg, &paths, &transport))
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
) -> Vec<FileCheckResult> {
    paths
        .iter()
        .map(|path| {
            let remote = walk::remote_join(&cfg.paths.remote_root, path);
            match transport.check(&cfg.connection.user, &cfg.connection.password, &remote) {
                Ok(result) => FileCheckResult {
                    path: path.clone(),
                    status: if result.ok {
                        FileCheckStatus::Passed
                    } else {
                        FileCheckStatus::Failed
                    },
                    diagnostics: result.diagnostics,
                },
                Err(error) => FileCheckResult {
                    path: path.clone(),
                    status: FileCheckStatus::TransportError(format!("{error:#}")),
                    diagnostics: String::new(),
                },
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
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    use anyhow::{anyhow, Result};

    use super::*;
    use crate::config::{Connection, Editor, Paths, Sync};
    use crate::udp::CheckResult;

    struct FakeTransport {
        responses: RefCell<VecDeque<Result<CheckResult>>>,
        paths: Rc<RefCell<Vec<String>>>,
    }

    impl FakeTransport {
        fn new(responses: impl IntoIterator<Item = Result<CheckResult>>) -> Self {
            Self {
                responses: RefCell::new(responses.into_iter().collect()),
                paths: Rc::new(RefCell::new(Vec::new())),
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
    fn check_with_factory_creates_one_transport_and_collects_ordered_results() {
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
        let checked_paths = transport.paths.clone();
        let factory_calls = Cell::new(0);

        let results = check_with_factory(
            &cfg,
            &[
                "one.c".to_string(),
                "two.c".to_string(),
                "three.c".to_string(),
            ],
            || {
                factory_calls.set(factory_calls.get() + 1);
                Ok(transport)
            },
        )
        .unwrap();

        assert_eq!(factory_calls.get(), 1);

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
            *checked_paths.borrow(),
            vec![
                "/mudlib/one.c".to_string(),
                "/mudlib/two.c".to_string(),
                "/mudlib/three.c".to_string(),
            ]
        );
    }

    #[test]
    fn check_with_factory_returns_empty_without_creating_transport() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path().to_path_buf());
        let factory_calls = Cell::new(0);

        let results = check_with_factory(&cfg, &[], || {
            factory_calls.set(factory_calls.get() + 1);
            Ok(FakeTransport::new(std::iter::empty()))
        })
        .unwrap();

        assert!(results.is_empty());
        assert_eq!(factory_calls.get(), 0);
    }

    #[test]
    fn check_with_factory_normalizes_safe_absolute_paths_and_validates_before_creation() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path().to_path_buf());
        let safe_absolute = root.path().join("nested/safe.c");
        std::fs::create_dir(root.path().join("nested")).unwrap();
        let transport = FakeTransport::new([Ok(CheckResult {
            ok: true,
            diagnostics: String::new(),
        })]);

        let results = check_with_factory(&cfg, &[safe_absolute.display().to_string()], || {
            Ok(transport)
        })
        .unwrap();
        assert_eq!(results[0].path, "nested/safe.c");

        let factory_calls = Cell::new(0);
        let outside = root.path().parent().unwrap().join("outside.c");
        let error = check_with_factory(
            &cfg,
            &["valid.c".to_string(), outside.display().to_string()],
            || {
                factory_calls.set(factory_calls.get() + 1);
                Ok(FakeTransport::new(std::iter::empty()))
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside local_root"));
        assert_eq!(factory_calls.get(), 0);
    }
}
