//! Typed exit conditions for `zed-ftp`.
//!
//! These are returned from command modules wrapped in [`anyhow::Error`] so
//! callers can continue to use `?` and `with_context(...)` freely while
//! `main()` keeps the ability to map the failure to a specific process exit
//! code (see [`crate`] docs and `src/main.rs`).
//!
//! The mapping intentionally distinguishes only the three categories Zed's
//! `tasks.json` cares about: conflicts (exit 2) versus config/auth problems
//! (exit 3). Everything else falls through to a generic exit 1.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Exit {
    /// A pull/push/sync was refused because the user would have lost work
    /// without an explicit `--force`. Maps to exit code 2.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The config file is missing, unreadable, or malformed. Maps to exit
    /// code 3.
    #[error("config: {0}")]
    Config(String),

    /// FTP connect/login failed. Maps to exit code 3 so Zed surfaces an
    /// auth panel rather than a generic error.
    #[error("auth: {0}")]
    Auth(String),
}
