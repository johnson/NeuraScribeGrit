//! Differential tests for the two index primitives behind `git rm -r --cached`.
//!
//! Both are checked against real `git`: `ls-files` for the read side, and
//! `rm --cached` for the write side. The point of the write test is the
//! work tree - git leaves the files on disk, and so must this.

use std::fs;
use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::index_ops;
use neurascribe_grit_ffi::ops;
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A repo with a tracked cache directory, a tracked sibling, and one untracked
/// file that exists on disk but was never added.
fn repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "t"]);
    git(root, &["config", "user.email", "t@example.com"]);
    write(root, "neurascribe.bundle/a.db", "a");
    write(root, "neurascribe.bundle/nested/b.db", "b");
    write(root, "notes.md", "n");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "init"]);
    // Written after the commit, so it exists on disk but is NOT in the index.
    write(root, "neurascribe-loose.txt", "untracked");
    dir
}

fn open(dir: &TempDir) -> Box<Repository> {
    Box::new(ops::open(dir.path()).unwrap())
}

#[test]
fn indexed_paths_under_matches_git_ls_files() {
    let dir = repo();
    let got = index_ops::indexed_paths_under(&open(&dir), "neurascribe.bundle").unwrap();
    let expected: Vec<String> = git(dir.path(), &["ls-files", "--", "neurascribe.bundle"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn a_directory_nothing_tracks_under_reports_empty() {
    let dir = repo();
    let got = index_ops::indexed_paths_under(&open(&dir), "neurascribe-loose").unwrap();
    assert!(got.is_empty(), "untracked directory reported {got:?}");
}

#[test]
fn a_sibling_with_a_shared_prefix_is_not_a_match() {
    let dir = repo();
    let got = index_ops::indexed_paths_under(&open(&dir), "neurascribe").unwrap();
    // neurascribe-loose.txt is not under neurascribe/.
    assert!(got.is_empty(), "prefix leaked across a component boundary: {got:?}");
}

#[test]
fn untrack_directory_matches_git_rm_cached() {
    let dir = repo();
    let removed = index_ops::untrack_directory(&open(&dir), "neurascribe.bundle").unwrap();
    assert_eq!(removed, 2, "nested files count too");

    // What git now thinks is tracked.
    let tracked: Vec<String> = git(dir.path(), &["ls-files"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(tracked, vec!["notes.md".to_string()]);

    // The files are still on disk, which is the whole point of --cached.
    assert!(dir.path().join("neurascribe.bundle/a.db").exists());
    assert!(dir.path().join("neurascribe.bundle/nested/b.db").exists());
}

#[test]
fn untracked_files_stay_untracked_after_untracking_the_directory() {
    let dir = repo();
    write(dir.path(), "neurascribe.bundle/fresh.db", "new");
    index_ops::untrack_directory(&open(&dir), "neurascribe.bundle").unwrap();
    let status = git(dir.path(), &["status", "--porcelain"]);
    // The tracked files became deletions; the never-added one is not restored.
    assert!(status.contains("D  neurascribe.bundle/a.db"), "{status}");
    assert!(
        !status.contains("A  neurascribe.bundle/fresh.db"),
        "a file that was never added came back: {status}"
    );
}

#[test]
fn untracking_nothing_is_a_no_op_not_an_error() {
    let dir = repo();
    assert_eq!(index_ops::untrack_directory(&open(&dir), "neurascribe-loose").unwrap(), 0);
}
#[test]
fn an_empty_directory_means_the_whole_index() {
    let dir = repo();
    let got = index_ops::indexed_paths_under(&open(&dir), "").unwrap();
    let expected: Vec<String> = git(dir.path(), &["ls-files"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(got, expected);
    // The never-added file is still not in there.
    assert!(!got.contains(&"neurascribe-loose.txt".to_string()));
}