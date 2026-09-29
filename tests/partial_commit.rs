//! `git commit -- <paths>` semantics.
//!
//! Committing a subset of what is staged is the case a file-picker UI produces,
//! and getting it wrong silently commits the user's other work. `git` is the
//! specification, so the key test compares against the binary directly.

use neurascribe_grit_ffi::ops;
use std::path::Path;
use std::process::Command;

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed (status {:?}): stdout={:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp");
    let root = dir.path();
    git(root, &["init", "--initial-branch=main"]);
    for (name, body) in files {
        std::fs::write(root.join(name), body).expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["-c", "user.email=t@example.com", "-c", "user.name=T", "commit", "-m", "seed"]);
    dir
}

/// A blob\'s content at `rev`, or None when the path is absent.
fn blob_at(root: &Path, rev: &str, path: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &format!("{rev}:{path}")])
        .output()
        .expect("git runs");
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        None
    }
}

/// The paths `git` considers staged against HEAD, sorted.
fn git_staged(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = git(root, &["diff", "--cached", "--name-only", "HEAD"])
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    v.sort();
    v
}

/// The same, read through grit, so the two can be compared.
fn grit_staged(root: &Path) -> Vec<String> {
    let repo = ops::open(root).expect("open");
    let mut v: Vec<String> = ops::staged_entries(&repo)
        .expect("staged")
        .into_iter()
        .map(|s| s.path)
        .collect();
    v.sort();
    v
}

/// Stage both files, then partially commit only `a.txt`.
fn edit_both_files(root: &Path) {
    std::fs::write(root.join("a.txt"), "one updated\n").expect("write");
    std::fs::write(root.join("b.txt"), "two updated\n").expect("write");
}

/// Stage both files, then partially commit only `a.txt`.
fn partial_commit_of_a(root: &Path) {
    edit_both_files(root);

    let repo = ops::open(root).expect("open");
    ops::stage_file(&repo, "b.txt").expect("stage b");
    ops::stage_file(&repo, "a.txt").expect("stage a");
    ops::commit_only(&repo, "Commit a only", "Test", "t@example.com", &["a.txt".to_string()])
        .expect("partial commit");
}

#[test]
fn a_partial_commit_agrees_with_real_git() {
    // The contract being copied is git\'s, so compare against the binary:
    // same commit tree, same index afterwards.
    let ours = repo_with(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
    let theirs = repo_with(&[("a.txt", "one\n"), ("b.txt", "two\n")]);

    partial_commit_of_a(ours.path());

    // The git side needs the same work-tree edits, or it has nothing to
    // commit and the comparison is vacuous.
    edit_both_files(theirs.path());
    git(theirs.path(), &["add", "."]);

    git(
        theirs.path(),
        &["-c", "user.email=t@example.com", "-c", "user.name=T", "commit", "-m", "Commit a only", "--", "a.txt"],
    );

    for path in ["a.txt", "b.txt"] {
        assert_eq!(
            blob_at(ours.path(), "HEAD", path),
            blob_at(theirs.path(), "HEAD", path),
            "{path} differs from what git produced"
        );
    }
    assert_eq!(grit_staged(ours.path()), git_staged(theirs.path()));
}

#[test]
fn a_partial_commit_carries_over_what_it_did_not_name() {
    // b.txt stays in the tree -- it was committed earlier -- but with its OLD
    // content, and its new content stays staged. A partial commit is not a
    // deletion of everything unmentioned.
    let dir = repo_with(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
    let root = dir.path();
    partial_commit_of_a(root);

    assert_eq!(blob_at(root, "HEAD", "a.txt").as_deref(), Some("one updated\n"));
    assert_eq!(
        blob_at(root, "HEAD", "b.txt").as_deref(),
        Some("two\n"),
        "b.txt must keep its committed content, not the staged one"
    );
    assert_eq!(grit_staged(root), vec!["b.txt".to_string()]);
    assert_eq!(
        std::fs::read_to_string(root.join("b.txt")).expect("read"),
        "two updated\n",
        "the work tree must not be touched"
    );
}

#[test]
fn a_staged_deletion_the_user_did_not_name_is_not_committed() {
    // The case a naive "filter the index" implementation gets wrong: b.txt is
    // staged as deleted. A partial commit of a.txt must not carry that over.
    let dir = repo_with(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
    let root = dir.path();
    std::fs::remove_file(root.join("b.txt")).expect("remove");
    git(root, &["rm", "--cached", "b.txt"]);
    std::fs::write(root.join("a.txt"), "one updated\n").expect("write");

    let repo = ops::open(root).expect("open");
    ops::stage_file(&repo, "a.txt").expect("stage a");
    ops::commit_only(&repo, "only a", "Test", "t@example.com", &["a.txt".to_string()])
        .expect("partial commit");

    assert_eq!(
        blob_at(root, "HEAD", "b.txt").as_deref(),
        Some("two\n"),
        "b.txt must stay in the commit that did not name it"
    );
    assert_eq!(
        grit_staged(root),
        vec!["b.txt".to_string()],
        "the deletion is still pending and must remain staged"
    );
}

#[test]
fn committing_a_path_that_is_not_staged_is_an_error() {
    // Otherwise the commit would be an empty one that looks like real work.
    let dir = repo_with(&[("a.txt", "one\n")]);
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "changed\n").expect("write");
    let repo = ops::open(root).expect("open");

    let err =
        ops::commit_only(&repo, "nothing", "Test", "t@example.com", &["a.txt".to_string()])
            .expect_err("a.txt is modified in the work tree but not staged");
    assert!(format!("{err:#}").contains("are staged"), "{err:#}");
}
