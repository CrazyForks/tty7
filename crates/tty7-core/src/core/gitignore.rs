//! The `.gitignore` chain a directory listing is scored against.
//!
//! One matcher is compiled per directory that has a `.gitignore`, cached by
//! that directory's path, and a path is scored by walking the chain from the
//! tree root down to the path's own parent — **the deepest match wins**, so a
//! nested `.gitignore`'s whitelist (`!pattern`) can un-ignore what an ancestor
//! ignored, which is what git itself does.
//!
//! Lives in `tty7-core` rather than beside the file tree because the answer has
//! to be identical on both sides of a remote workspace: the GUI dims ignored
//! entries for a local tree, and the server has to dim exactly the same ones
//! for a remote tree. One implementation, no drift.
//!
//! Compiling is lazy and cached (including the negative case — a directory with
//! no `.gitignore` caches as `None`), so a chain that is carried across
//! listings pays for each directory once. `Arc`, so a chain can be cloned onto
//! a background thread and its compiled matchers shared rather than rebuilt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::Gitignore;

/// Compiled `.gitignore` matchers, keyed by the directory each came from
/// (`None` = that directory has no `.gitignore`).
#[derive(Default, Clone)]
pub struct GitignoreChain {
    matchers: HashMap<PathBuf, Option<Arc<Gitignore>>>,
}

impl GitignoreChain {
    /// Walk the `.gitignore` chain from `root` down to `path`'s directory and
    /// report whether `path` ends up ignored; the deepest match wins
    /// (whitelist `!patterns` un-ignore).
    ///
    /// `is_dir` matters because gitignore patterns can be directory-only
    /// (`build/`). Paths outside `root` simply score against nothing.
    pub fn is_ignored(&mut self, path: &Path, is_dir: bool, root: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        let mut state = false;
        // Ancestor chain root → parent, in order.
        let mut chain: Vec<&Path> = parent
            .ancestors()
            .take_while(|a| a.starts_with(root))
            .collect();
        chain.reverse();
        for dir in chain {
            let gi = self
                .matchers
                .entry(dir.to_path_buf())
                .or_insert_with(|| {
                    let file = dir.join(".gitignore");
                    file.is_file().then(|| {
                        let (gi, _err) = Gitignore::new(&file);
                        Arc::new(gi)
                    })
                })
                .clone();
            let Some(gi) = gi else { continue };
            let Ok(rel) = path.strip_prefix(dir) else {
                continue;
            };
            match gi.matched(rel, is_dir) {
                ignore::Match::Ignore(_) => state = true,
                ignore::Match::Whitelist(_) => state = false,
                ignore::Match::None => {}
            }
        }
        state
    }

    /// Fold another chain's compiled matchers in — how a background listing
    /// hands back the ones it had to compile so the next listing re-uses them.
    pub fn absorb(&mut self, other: Self) {
        self.matchers.extend(other.matchers);
    }

    /// Drop every compiled matcher, so the next scoring recompiles from disk.
    /// The invalidation a `.gitignore` edit triggers.
    pub fn clear(&mut self) {
        self.matchers.clear();
    }

    /// How many directories have been scored (and so cached) so far — the
    /// negative entries for directories without a `.gitignore` included.
    pub fn len(&self) -> usize {
        self.matchers.len()
    }

    /// Whether nothing has been compiled or cached yet.
    pub fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a `.gitignore` into `dir` (creating it) with the given patterns.
    fn write_ignore(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(".gitignore"), body).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tty7-gitignore-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The deepest `.gitignore` wins, so a nested whitelist un-ignores what the
    /// root ignored — the rule the file tree's dimming depends on.
    #[test]
    fn the_deepest_match_wins() {
        let root = scratch("deepest");
        write_ignore(&root, "*.log\n");
        write_ignore(&root.join("keep"), "!important.log\n");

        let mut chain = GitignoreChain::default();
        assert!(chain.is_ignored(&root.join("a.log"), false, &root));
        assert!(chain.is_ignored(&root.join("keep/other.log"), false, &root));
        assert!(!chain.is_ignored(&root.join("keep/important.log"), false, &root));
        assert!(!chain.is_ignored(&root.join("a.txt"), false, &root));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory-only pattern (`build/`) matches the directory, not a file of
    /// the same name — which is why scoring takes `is_dir`.
    #[test]
    fn directory_only_patterns_need_is_dir() {
        let root = scratch("dironly");
        write_ignore(&root, "build/\n");

        let mut chain = GitignoreChain::default();
        assert!(chain.is_ignored(&root.join("build"), true, &root));
        assert!(!chain.is_ignored(&root.join("build"), false, &root));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `clear` forces a recompile, so an edited `.gitignore` takes effect;
    /// without it the cached matcher would answer from the old patterns.
    #[test]
    fn clear_lets_an_edited_gitignore_take_effect() {
        let root = scratch("clear");
        write_ignore(&root, "*.log\n");

        let mut chain = GitignoreChain::default();
        assert!(chain.is_ignored(&root.join("a.log"), false, &root));

        write_ignore(&root, "*.tmp\n");
        assert!(
            chain.is_ignored(&root.join("a.log"), false, &root),
            "cached"
        );
        chain.clear();
        assert!(!chain.is_ignored(&root.join("a.log"), false, &root));
        assert!(chain.is_ignored(&root.join("a.tmp"), false, &root));

        let _ = std::fs::remove_dir_all(&root);
    }
}
