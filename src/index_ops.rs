//! Index-level operations that git porcelain does not expose but an app needs.
//!
//! The motivating case is `git rm -r --cached <dir>`: stop tracking a tree
//! while leaving every file on disk. grit has no `rm` porcelain, and the gix
//! FFI this replaces had no equivalent either, so the workspace .gitignore
//! guard could only untrack app cache files when the app was running on macOS
//! with git installed. Two primitives, one per question the guard asks:
//!
//! * [`indexed_paths_under`] - what does the index track below this directory?
//! * [`untrack_directory`] - drop those entries, keeping the work tree.

use anyhow::Result;
use grit_lib::index::Index;
use grit_lib::repo::Repository;

/// Paths the index tracks strictly below `directory`, sorted.
///
/// Unlike a directory listing this cannot be fooled by files that exist on
/// disk but were never added, which is the whole point of asking.
///
/// An empty `directory` returns every tracked path - the index as a whole.
/// That is deliberately not the same as "the repository root": there is no
/// other way to enumerate the index at all, since `list_files` reads a commit
/// tree and the tree and the index routinely disagree mid-edit.
pub fn indexed_paths_under(repo: &Repository, directory: &str) -> Result<Vec<String>> {
    let prefix = normalize(directory);
    let index = repo.load_index()?;
    let mut paths: Vec<String> = index
        .entries
        .iter()
        // Stage 0 only: a conflicted path appears in the index once per stage,
        // and reporting it twice would make the caller untrack it twice.
        .filter(|entry| entry.stage() == 0)
        .filter_map(|entry| String::from_utf8(entry.path.clone()).ok())
        .filter(|path| is_below(path, &prefix))
        .collect();
    paths.sort();
    Ok(paths)
}

/// Remove every index entry under `directory`, leaving the files in place.
///
/// This is `git rm -r --cached`: the work tree is untouched, so the files
/// stay where they are and simply stop being tracked. Returns how many
/// entries were dropped, so the caller can tell a no-op from a failure.
pub fn untrack_directory(repo: &Repository, directory: &str) -> Result<usize> {
    let prefix = normalize(directory);
    let mut index = repo.load_index()?;
    let removed = index
        .entries
        .iter()
        .filter(|entry| entry.stage() == 0)
        .filter(|entry| is_below(&String::from_utf8_lossy(&entry.path), &prefix))
        .count();
    if removed == 0 {
        return Ok(0);
    }
    index.remove_descendants_under_path(&prefix);
    write_index(repo, &mut index)?;
    Ok(removed)
}

/// Git stores directory paths without a leading or trailing slash.
fn normalize(raw: &str) -> String {
    raw.trim_matches('/').to_string()
}

/// Whether `path` sits strictly below `prefix` at a component boundary.
///
/// The boundary check is the whole point: `vendor/lib-extra` must not match
/// `vendor/lib`.
///
/// An empty `prefix` is the whole tree - see [`indexed_paths_under`].
fn is_below(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return !path.is_empty();
    }
    path.len() > prefix.len()
        && path.starts_with(prefix)
        && path.as_bytes()[prefix.len()] == b'/'
}

fn write_index(repo: &Repository, index: &mut Index) -> Result<()> {
    index.clear_cache_tree();
    repo.write_index(index)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_respects_component_boundaries() {
        assert!(is_below("a/b", "a"));
        assert!(is_below("a/b/c", "a/b"));
        assert!(!is_below("ab", "a"));
        assert!(!is_below("a-b/c", "a"));
        assert!(!is_below("a", "a"));
        // An empty prefix is the whole tree.
        assert!(is_below("anything/at/all", ""));
        assert!(!is_below("", ""));
    }

    #[test]
    fn normalize_strips_slashes() {
        assert_eq!(normalize("/a/b/"), "a/b");
        assert_eq!(normalize("a/b"), "a/b");
    }
}