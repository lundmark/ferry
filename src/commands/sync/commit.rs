// `is_current` and cancellation are consumed by the scoped/LSP guards in later tasks.
#![allow(dead_code)]

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitDecision {
    Committed,
    Cancelled,
}

pub trait CommitGate: Send + Sync {
    fn is_current(&self) -> bool;

    /// Atomically order `mutation` against invalidation.
    ///
    /// Implementations must invoke `mutation` at most once.
    fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision>;
}

pub struct UnconditionalCommitGate;

impl CommitGate for UnconditionalCommitGate {
    fn is_current(&self) -> bool {
        true
    }

    fn commit(&self, mutation: &mut dyn FnMut() -> Result<()>) -> Result<CommitDecision> {
        mutation()?;
        Ok(CommitDecision::Committed)
    }
}

#[cfg(test)]
mod tests {
    use super::{CommitDecision, CommitGate, UnconditionalCommitGate};

    #[test]
    fn directory_snapshots_keep_relative_local_and_remote_expectations() {
        let snapshots = crate::commands::sync::ExpectedDirectorySnapshots {
            relative: "nested".to_string(),
            local: crate::commands::sync::ExpectedLocalDirectory::Missing,
            remote: crate::commands::file_transfer::RemoteDestinationSnapshot::Missing,
        };

        assert_eq!(snapshots.relative, "nested");
        assert_eq!(
            snapshots.local,
            crate::commands::sync::ExpectedLocalDirectory::Missing
        );
        assert_eq!(
            snapshots.remote,
            crate::commands::file_transfer::RemoteDestinationSnapshot::Missing
        );
    }

    #[test]
    fn unconditional_gate_invokes_the_mutation_once_and_commits() {
        let gate = UnconditionalCommitGate;
        let mut calls = 0;

        let decision = gate
            .commit(&mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();

        assert_eq!(decision, CommitDecision::Committed);
        assert_eq!(calls, 1);
        assert!(gate.is_current());
    }

    #[test]
    fn unconditional_gate_propagates_mutation_errors_without_retrying() {
        let gate = UnconditionalCommitGate;
        let mut calls = 0;

        let error = gate
            .commit(&mut || {
                calls += 1;
                anyhow::bail!("mutation failed")
            })
            .unwrap_err();

        assert_eq!(calls, 1);
        assert!(format!("{error:#}").contains("mutation failed"));
    }
}
