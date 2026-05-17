#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FileState {
    InSync,
    LocalChanged,
    RemoteChanged,
    BothChanged,   // conflict
    LocalOnly,
    RemoteOnly,
    Untracked,     // exists both, no known hash
}

/// Pure classification. Inputs are hashes; `None` means file does not exist (local/remote)
/// or has never been synced (known).
pub fn classify(local: Option<&str>, remote: Option<&str>, known: Option<&str>) -> FileState {
    match (local, remote, known) {
        (None, None, _) => unreachable!("called with no file present"),
        (Some(_), None, _) => FileState::LocalOnly,
        (None, Some(_), _) => FileState::RemoteOnly,
        (Some(l), Some(r), Some(k)) if l == r && r == k => FileState::InSync,
        (Some(l), Some(r), Some(k)) if l != k && r == k => FileState::LocalChanged,
        (Some(l), Some(r), Some(k)) if l == k && r != k => FileState::RemoteChanged,
        (Some(l), Some(r), Some(k)) if l != k && r != k => {
            if l == r { FileState::InSync } else { FileState::BothChanged }
        }
        (Some(_), Some(_), None) => FileState::Untracked,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn in_sync()          { assert_eq!(classify(Some("a"), Some("a"), Some("a")), FileState::InSync); }
    #[test] fn local_changed()    { assert_eq!(classify(Some("b"), Some("a"), Some("a")), FileState::LocalChanged); }
    #[test] fn remote_changed()   { assert_eq!(classify(Some("a"), Some("b"), Some("a")), FileState::RemoteChanged); }
    #[test] fn both_changed()     { assert_eq!(classify(Some("b"), Some("c"), Some("a")), FileState::BothChanged); }
    #[test] fn both_changed_same() { assert_eq!(classify(Some("b"), Some("b"), Some("a")), FileState::InSync); }
    #[test] fn local_only()       { assert_eq!(classify(Some("a"), None, None), FileState::LocalOnly); }
    #[test] fn remote_only()      { assert_eq!(classify(None, Some("a"), None), FileState::RemoteOnly); }
    #[test] fn untracked()        { assert_eq!(classify(Some("a"), Some("a"), None), FileState::Untracked); }
    #[test] fn untracked_differ() { assert_eq!(classify(Some("a"), Some("b"), None), FileState::Untracked); }
}
