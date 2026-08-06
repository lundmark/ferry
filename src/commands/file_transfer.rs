#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    Unchanged,
    Transferred,
    SkippedMissingSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub path: String,
    pub status: TransferStatus,
}

impl TransferOutcome {
    pub fn new(path: &str, status: TransferStatus) -> Self {
        Self {
            path: path.to_string(),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TransferOutcome, TransferStatus};

    #[test]
    fn outcome_keeps_the_relative_path_and_status() {
        assert_eq!(
            TransferOutcome::new("src/main.rs", TransferStatus::Transferred),
            TransferOutcome {
                path: "src/main.rs".to_string(),
                status: TransferStatus::Transferred,
            }
        );
    }
}
