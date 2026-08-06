use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

pub struct Matcher {
    gi: Gitignore,
}

impl Matcher {
    pub fn new(patterns: &[String], root: &Path) -> anyhow::Result<Self> {
        let mut b = GitignoreBuilder::new(root);
        for p in patterns {
            b.add_line(None, p)?;
        }
        Ok(Self { gi: b.build()? })
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.gi
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(patterns: &[&str]) -> Matcher {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        Matcher::new(&owned, Path::new("/work")).unwrap()
    }

    #[test]
    fn ignores_node_modules() {
        let mat = m(&["node_modules/", "*.log"]);
        assert!(mat.is_ignored(Path::new("/work/node_modules/foo"), false));
        assert!(mat.is_ignored(Path::new("/work/server.log"), false));
        assert!(!mat.is_ignored(Path::new("/work/src/index.html"), false));
    }

    #[test]
    fn negation_pattern() {
        // Sanity: ignore-then-unignore the negation case the design relies on.
        let mat = m(&["*.log", "!keep.log"]);
        assert!(mat.is_ignored(Path::new("/work/server.log"), false));
        assert!(!mat.is_ignored(Path::new("/work/keep.log"), false));
    }

    #[test]
    fn directory_only_pattern_requires_is_dir() {
        // `build/` matches the directory itself only when is_dir is true,
        // but files beneath it always match via parent walk.
        let mat = m(&["build/"]);
        assert!(!mat.is_ignored(Path::new("/work/build"), false));
        assert!(mat.is_ignored(Path::new("/work/build"), true));
        assert!(mat.is_ignored(Path::new("/work/build/out.o"), false));
    }
}
