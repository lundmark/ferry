#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Apply,
    DryRun,
}

impl ExecutionMode {
    pub fn from_dry_run(dry_run: bool) -> Self {
        if dry_run { Self::DryRun } else { Self::Apply }
    }

    pub fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }

    pub fn should_apply(self) -> bool {
        matches!(self, Self::Apply)
    }
}

#[cfg(test)]
mod execution_mode_tests {
    use super::ExecutionMode;

    #[test]
    fn maps_cli_flag_and_reports_write_permission() {
        assert_eq!(ExecutionMode::from_dry_run(false), ExecutionMode::Apply);
        assert_eq!(ExecutionMode::from_dry_run(true), ExecutionMode::DryRun);
        assert!(ExecutionMode::Apply.should_apply());
        assert!(!ExecutionMode::DryRun.should_apply());
    }
}

pub mod cc;
pub mod hook;
pub mod init;
pub mod ls;
pub mod pull;
pub mod push;
pub mod remote_hash;
pub mod rm;
pub mod status;
pub mod sync;
pub mod walk;
