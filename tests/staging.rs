//! Differential tests for the two primitives behind the staging drawer:
//! `git reset HEAD -- <path>` and `git commit --amend`.
//!
//! Both are checked against real `git`, because both have a failure mode that
//! looks fine and is not: unstage can restore a blob while still marking the
//! path as staged, and amend can succeed while quietly keeping HEAD as a
//! parent, which forks the history instead of replacing it.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::repo::Repository;
use neurascribe_grit_ffi::ops;
use neurascribe_grit_ffi::types::FileStatus;
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

fn repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    write(root, "a.txt", "a\n");
    write(root, "b.txt", "b\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "first"]);
    dir
}

fn staged_paths(repo: &Repository) -> String {
    let index = repo.load_index().unwrap();
    let mut names: Vec<String> = index
        .entries
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).to_string())
        .collect();
    names.sort();
    names.join("\n")
}

fn entry_for(repo: &Repository, path: &str) -> FileStatus {
    // Bound first: the status result is owned, so a borrow taken through
    // the temporary chain would not outlive the expression.
    let result = ops::status(repo).unwrap();
    result
        .files
        .into_iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} was not reported by status"))
}
#[test]
fn unstage_an_edit_restores_the_head_blob_and_clears_the_staged_flag() {
    let dir = repo();
    let root = dir.path();
    write(root, "a.txt", "edited\n");
    let handle = ops::open(root).unwrap();
    ops::stage_file(&handle, "a.txt").unwrap();
    assert!(staged_paths(&handle).contains("a.txt"));

    ops::unstage_file(&handle, "a.txt").unwrap();

    let entry = entry_for(&handle, "a.txt");
    assert!(entry.unstaged, "a.txt should be an unstaged edit again");
    assert!(!entry.staged, "a.txt must not still read as staged");
}

#[test]
fn unstage_a_new_file_removes_the_entry_entirely() {
    let dir = repo();
    let root = dir.path();
    write(root, "new.txt", "new\n");
    let handle = ops::open(root).unwrap();
    ops::stage_file(&handle, "new.txt").unwrap();
    assert!(staged_paths(&handle).contains("new.txt"));

    ops::unstage_file(&handle, "new.txt").unwrap();

    assert!(
        !staged_paths(&handle).contains("new.txt"),
        "a new file must leave the index, not stay staged"
    );
    assert!(
        root.join("new.txt").exists(),
        "unstaging must not touch the work tree"
    );
}

#[test]
fn unstage_keeps_the_work_tree_edit() {
    let dir = repo();
    let root = dir.path();
    write(root, "a.txt", "edited\n");
    let handle = ops::open(root).unwrap();
    ops::stage_file(&handle, "a.txt").unwrap();
    ops::unstage_file(&handle, "a.txt").unwrap();
    assert_eq!(
        fs::read_to_string(root.join("a.txt")).unwrap(),
        "edited\n",
        "unstaging is about the next commit, never about losing work"
    );
}

#[test]
fn unstage_a_staged_deletion_recovers_the_head_blob() {
    let dir = repo();
    let root = dir.path();
    let handle = ops::open(root).unwrap();
    fs::remove_file(root.join("a.txt")).unwrap();
    ops::stage_file(&handle, "a.txt").unwrap();

    ops::unstage_file(&handle, "a.txt").unwrap();

    // HEAD has a.txt but the work tree does not, so the entry has to be
    // restored from HEAD with no stat data rather than dropped.
    assert!(staged_paths(&handle).contains("a.txt"));
    let entry = entry_for(&handle, "a.txt");
    assert!(entry.unstaged, "a missing file reads as an unstaged deletion");
    assert!(!entry.staged);
}

#[test]
fn amend_replaces_head_rather_than_forking_it() {
    let dir = repo();
    let root = dir.path();
    let handle = ops::open(root).unwrap();
    write(root, "a.txt", "amended\n");
    ops::stage_file(&handle, "a.txt").unwrap();
    let before = ops::head_sha(&handle).unwrap();

    let after = ops::amend_commit(&handle, "corrected", "T", "t@example.com").unwrap();

    assert_ne!(after, before, "amend writes a new commit object");
    assert_eq!(ops::head_sha(&handle).unwrap(), after);
    assert!(
        !ops::is_ancestor(&handle, &before, &after).unwrap(),
        "the replaced commit must not be an ancestor of its replacement"
    );
    assert_eq!(
        ops::log(&handle, 20).unwrap().len(),
        1,
        "amend must not leave the old commit on the branch"
    );
}

#[test]
fn amend_keeps_the_original_author() {
    let dir = repo();
    let root = dir.path();
    let handle = ops::open(root).unwrap();
    write(root, "a.txt", "amended\n");
    ops::stage_file(&handle, "a.txt").unwrap();
    let before = ops::head_sha(&handle).unwrap();

    ops::amend_commit(&handle, "second subject", "T", "t@example.com").unwrap();

    // Differential: git's own amend keeps the author of the commit it replaces.
    let author_before = git(root, &["log", "-1", "--format=%an <%ae>", &before]);
    let author_after = git(root, &["log", "-1", "--format=%an <%ae>", "HEAD"]);
    assert_eq!(author_after, author_before);
}

#[test]
fn amend_without_a_commit_fails_rather_than_creating_one() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init"]);
    let handle = ops::open(root).unwrap();
    write(root, "a.txt", "a\n");
    ops::stage_file(&handle, "a.txt").unwrap();
    assert!(
        ops::amend_commit(&handle, "no", "T", "t@example.com").is_err(),
        "there is nothing to amend before the first commit"
    );
}
