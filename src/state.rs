#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[must_use]
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
///
/// Caller contract: never invoke with `local == None && remote == None`. A path that
/// exists in neither place should be pruned from the union walk, not classified.
#[must_use]
pub fn classify(local: Option<&str>, remote: Option<&str>, known: Option<&str>) -> FileState {
    match (local, remote, known) {
        (None, None, _) => unreachable!("called with no file present"),
        (Some(_), None, _) => FileState::LocalOnly,
        (None, Some(_), _) => FileState::RemoteOnly,
        (Some(_), Some(_), None) => FileState::Untracked,
        (Some(l), Some(r), Some(k)) => match (l == k, r == k, l == r) {
            (true,  true,  _    ) => FileState::InSync,
            (false, true,  _    ) => FileState::LocalChanged,
            (true,  false, _    ) => FileState::RemoteChanged,
            (false, false, true ) => FileState::InSync,     // both moved, same target
            (false, false, false) => FileState::BothChanged,
        }
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
