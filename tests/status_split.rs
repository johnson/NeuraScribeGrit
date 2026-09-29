//! Status must keep the staged / worktree split that the app's GitStatus needs.
//!
//! git prints two columns per path. An earlier version of the shim emitted one
//! row per column and then deduplicated by path, which silently dropped the
//! staged/unstaged distinction for any file that was both.

use std::fs;
use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::ops;
use neurascribe_grit_ffi::types::FileStatus;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
}

fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@t.local"]);
    write(root, "base.md", "base\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    dir
}

fn status_of(dir: &TempDir) -> std::collections::BTreeMap<String, FileStatus> {
    let repo = ops::open(dir.path()).unwrap();
    ops::status(&repo)
        .unwrap()
        .files
        .into_iter()
        .map(|f| (f.path.clone(), f))
        .collect()
}

#[test]
fn status_separates_staged_from_worktree_changes() {
    let dir = fixture();
    let root = dir.path();

    // staged, untouched since
    write(root, "staged.md", "one\n");
    git(root, &["add", "staged.md"]);

    // staged, then edited again: both flags must be set
    write(root, "both.md", "one\n");
    git(root, &["add", "both.md"]);
    write(root, "both.md", "two\n");

    // A TRACKED file edited in the work tree. A brand-new file would be
    // untracked rather than an unstaged modification, which is what the
    // untracked test below pins.
    write(root, "base.md", "edited in the work tree\n");

    let by_path = status_of(&dir);

    let staged = &by_path["staged.md"];
    assert!(staged.staged, "staged.md was added");
    assert!(!staged.unstaged, "staged.md was not edited after staging");

    let both = &by_path["both.md"];
    assert!(both.staged, "both.md was added");
    assert!(
        both.unstaged,
        "both.md was edited after staging, so it is also a worktree change"
    );

    let worktree_only = &by_path["base.md"];
    assert!(!worktree_only.staged, "base.md was never staged");
    assert!(worktree_only.unstaged);
    assert_eq!(worktree_only.status, "modified");
}

#[test]
fn a_worktree_deletion_is_unstaged_and_says_deleted() {
    let dir = fixture();
    let root = dir.path();
    write(root, "gone.md", "one\n");
    git(root, &["add", "gone.md"]);
    git(root, &["commit", "-q", "-m", "add gone"]);
    fs::remove_file(root.join("gone.md")).unwrap();

    let by_path = status_of(&dir);
    let gone = &by_path["gone.md"];
    assert!(gone.unstaged);
    assert!(!gone.staged, "a worktree deletion is not staged until it is added");
    assert_eq!(gone.status, "deleted");
}

#[test]
fn staging_a_deletion_moves_it_from_unstaged_to_staged() {
    let dir = fixture();
    let root = dir.path();
    write(root, "gone.md", "one\n");
    git(root, &["add", "gone.md"]);
    git(root, &["commit", "-q", "-m", "add gone"]);
    fs::remove_file(root.join("gone.md")).unwrap();
    git(root, &["add", "-A"]);

    let by_path = status_of(&dir);
    let gone = &by_path["gone.md"];
    assert!(gone.staged, "git add -A records the deletion in the index");
    assert!(!gone.unstaged, "the work tree now matches the index");
    assert_eq!(gone.status, "deleted");
}

#[test]
fn status_reports_a_path_once_not_once_per_column() {
    let dir = fixture();
    let root = dir.path();
    write(root, "both.md", "one\n");
    git(root, &["add", "both.md"]);
    write(root, "both.md", "two\n");

    let repo = ops::open(root).unwrap();
    let files = ops::status(&repo).unwrap().files;
    let both: Vec<_> = files.iter().filter(|f| f.path == "both.md").collect();

    assert_eq!(
        both.len(),
        1,
        "one row carrying both flags, not one row per column"
    );
    assert!(both[0].staged && both[0].unstaged);
}

#[test]
fn status_sorts_by_path() {
    let dir = fixture();
    let root = dir.path();
    for name in ["zebra.md", "apple.md", "mango.md"] {
        write(root, name, "x\n");
    }

    let repo = ops::open(root).unwrap();
    let paths: Vec<String> = ops::status(&repo)
        .unwrap()
        .files
        .into_iter()
        .map(|f| f.path)
        .collect();

    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
}

#[test]
fn untracked_is_neither_staged_nor_unstaged() {
    let dir = fixture();
    let root = dir.path();
    write(root, "fresh.md", "new\n");

    let by_path = status_of(&dir);
    let fresh = &by_path["fresh.md"];
    assert_eq!(fresh.status, "untracked");
    assert!(!fresh.staged);
    assert!(
        !fresh.unstaged,
        "an untracked file has no index entry to differ from"
    );
}

#[test]
fn a_clean_tree_reports_no_files() {
    let dir = fixture();
    assert!(status_of(&dir).is_empty());
}
