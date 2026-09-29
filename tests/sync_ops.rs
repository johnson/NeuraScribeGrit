//! Differential tests for the four ops added to replace the iOS hand-rolled
//! readers: ref resolution, ls-tree, single-path staging, and fast-forward.
//!
//! Each is checked against real `git`. The hand-rolled readers these replace
//! could not be tested that way, which is precisely why they drifted.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::ops;
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

/// git that reports success as a bool instead of asserting, for commands whose
/// exit code IS the answer (merge-base --is-ancestor).
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git")
        .status
        .success()
}

fn commit_all(dir: &Path, message: &str) -> String {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

/// Nested files, an executable, a nested directory, and two commits.
fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tester"]);
    git(root, &["config", "user.email", "t@example.com"]);

    fs::create_dir_all(root.join("notes/sub")).unwrap();
    fs::write(root.join("README.md"), "readme\n").unwrap();
    fs::write(root.join("notes/one.md"), "one\n").unwrap();
    fs::write(root.join("notes/sub/two.md"), "two\n").unwrap();
    fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    commit_all(root, "first");

    fs::write(root.join("notes/one.md"), "one changed\n").unwrap();
    commit_all(root, "second");

    dir
}

// ---------------------------------------------------------------- resolve_ref

#[test]
fn resolve_ref_matches_git_for_head_and_sha() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();
    let head = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();

    assert_eq!(ops::resolve_ref(&repo, "HEAD").unwrap(), head);
    // A raw SHA is not a ref; callers pass both, so both have to work.
    assert_eq!(ops::resolve_ref(&repo, &head).unwrap(), head);
    assert_eq!(
        ops::resolve_ref(&repo, "refs/heads/main").unwrap(),
        head,
        "full refname"
    );
}

#[test]
fn resolve_ref_finds_a_packed_ref() {
    let dir = fixture();
    let root = dir.path();
    // Packing moves every ref out of a loose file, which is exactly the case
    // the app's hand-rolled reader had to special-case.
    git(root, &["pack-refs", "--all"]);
    assert!(
        !root.join(".git/refs/remotes").exists(),
        "fixture should have no loose refs left"
    );

    let sha = git(root, &["rev-parse", "refs/heads/main"]).trim().to_string();
    git(
        root,
        &[
            "update-ref",
            "refs/remotes/origin/main",
            &sha,
        ],
    );
    git(root, &["pack-refs", "--all"]);

    let repo = ops::open(root).unwrap();
    assert_eq!(ops::resolve_ref(&repo, "refs/remotes/origin/main").unwrap(), sha);
    assert_eq!(ops::resolve_ref(&repo, "refs/heads/main").unwrap(), sha);
}

#[test]
fn resolve_ref_rejects_a_sha_that_is_not_an_object() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();
    let ghost = "0".repeat(40);
    assert!(
        ops::resolve_ref(&repo, &ghost).is_err(),
        "a well-formed SHA for a missing object must not resolve"
    );
    assert!(ops::resolve_ref(&repo, "refs/heads/nope").is_err());
}

// ------------------------------------------------------------------ list_files

#[test]
fn list_files_matches_ls_tree_recursive() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();
    let head = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();

    let want: Vec<String> = git(dir.path(), &["ls-tree", "-r", "--name-only", &head])
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    let got = ops::list_files(&repo, &head).unwrap();
    assert_eq!(got, want);
    // Nested paths are the point of -r.
    assert!(got.iter().any(|p| p == "notes/sub/two.md"));
    assert!(got.windows(2).all(|w| w[0] <= w[1]), "must be sorted");
}

#[test]
fn list_files_reflects_earlier_commits() {
    let dir = fixture();
    let root = dir.path();
    let first = git(root, &["rev-parse", "HEAD~1"]).trim().to_string();
    let repo = ops::open(root).unwrap();

    // notes/one.md changed in the second commit; the file set did not, but a
    // file added later must not appear in the earlier tree.
    fs::write(root.join("later.md"), "later\n").unwrap();
    commit_all(root, "third");

    let got = ops::list_files(&repo, &first).unwrap();
    assert!(!got.iter().any(|p| p == "later.md"));
    let now = ops::list_files(&repo, &ops::head_sha(&repo).unwrap()).unwrap();
    assert!(now.iter().any(|p| p == "later.md"));
}

// ------------------------------------------------------------------ stage_file

/// Staged entries as "mode path" lines, from git's own index.
fn git_staged(dir: &Path) -> Vec<String> {
    git(dir, &["diff", "--cached", "--name-status"])
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

#[test]
fn stage_file_records_a_modification_like_git_add() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();

    fs::write(root.join("notes/one.md"), "edited\n").unwrap();
    ops::stage_file(&repo, "notes/one.md").unwrap();

    assert_eq!(git_staged(root), vec!["M\tnotes/one.md"]);
    // The index must also be what a commit would actually record.
    assert_eq!(
        git(root, &["show", ":notes/one.md"]).trim(),
        "edited"
    );
}

#[test]
fn stage_file_records_a_new_file() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();

    fs::write(root.join("fresh.md"), "new\n").unwrap();
    ops::stage_file(&repo, "fresh.md").unwrap();

    assert_eq!(git_staged(root), vec!["A\tfresh.md"]);
}

#[test]
fn stage_file_records_a_deletion() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();

    fs::remove_file(root.join("notes/sub/two.md")).unwrap();
    ops::stage_file(&repo, "notes/sub/two.md").unwrap();

    assert_eq!(git_staged(root), vec!["D\tnotes/sub/two.md"]);
}

#[test]
fn stage_file_preserves_the_executable_bit() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();

    // Flip run.sh from 755 to 644 and stage it; the index must record the new
    // mode, or a checkout elsewhere would restore it as executable.
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    ops::stage_file(&repo, "run.sh").unwrap();

    let mode = git(root, &["ls-files", "-s", "run.sh"]);
    assert!(
        mode.starts_with("100644"),
        "expected a non-executable index entry, got: {}",
        mode.trim()
    );
    assert_eq!(git_staged(root), vec!["M\trun.sh"]);
}

#[test]
fn stage_file_follows_a_symlink_to_its_target() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();

    std::os::unix::fs::symlink("README.md", root.join("link.md")).unwrap();
    ops::stage_file(&repo, "link.md").unwrap();

    // git records mode 120000 and the link target, not the file contents.
    let staged = git(root, &["ls-files", "-s", "link.md"]);
    assert!(
        staged.starts_with("120000"),
        "expected a symlink index entry, got: {}",
        staged.trim()
    );
    assert_eq!(git(root, &["show", ":link.md"]).trim(), "README.md");
}

#[test]
fn staging_one_path_leaves_the_others_alone() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();

    fs::write(root.join("a.md"), "a\n").unwrap();
    fs::write(root.join("b.md"), "b\n").unwrap();
    ops::stage_file(&repo, "a.md").unwrap();

    assert_eq!(git_staged(root), vec!["A\ta.md"]);
}

// ------------------------------------------------------------- fast_forward_to

#[test]
fn fast_forward_moves_head_and_worktree() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();

    let target = ops::head_sha(&repo).unwrap();
    // An upstream commit on top of HEAD.
    fs::write(root.join("upstream.md"), "from upstream\n").unwrap();
    let tip = commit_all(root, "upstream commit");

    ops::fast_forward_to(&repo, &tip).unwrap();

    assert_eq!(ops::head_sha(&repo).unwrap(), tip);
    assert!(root.join("upstream.md").exists(), "work tree not updated");
    assert_eq!(fs::read_to_string(root.join("upstream.md")).unwrap(), "from upstream\n");
    assert!(!ops::status(&repo).unwrap().has_changes, "index should match HEAD");
    let _ = target;
}

#[test]
fn fast_forward_refuses_diverged_histories() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();

    // Real divergence: two commits that both have the same parent, on two
    // branches. HEAD is on one, the tip is the other, so neither is an ancestor
    // of the other. A reset-based setup is NOT this — it leaves a linear
    // history that fast-forwards happily, and the test would pass for the wrong
    // reason.
    let base = ops::head_sha(&repo).unwrap();

    git(root, &["checkout", "-q", "-b", "side"]);
    fs::write(root.join("side.md"), "side\n").unwrap();
    let tip = commit_all(root, "side only");

    git(root, &["checkout", "-q", "main"]);
    fs::write(root.join("main.md"), "main\n").unwrap();
    let before = commit_all(root, "main only");

    assert_ne!(tip, before);
    assert_eq!(ops::head_sha(&repo).unwrap(), before);
    // Prove the divergence with git itself, so a passing test cannot be
    // explained by the setup accidentally being linear.
    assert!(
        !git_ok(root, &["merge-base", "--is-ancestor", &before, &tip]),
        "fixture is not actually diverged: {before} IS an ancestor of {tip}"
    );

    let err = ops::fast_forward_to(&repo, &tip)
        .expect_err("a diverged history must not fast-forward");
    assert!(
        format!("{err:#}").contains("not a fast-forward"),
        "unhelpful error: {err:#}"
    );
    assert_eq!(ops::head_sha(&repo).unwrap(), before, "HEAD must not move");
    let _ = base;
}

#[test]
fn fast_forward_to_the_current_head_is_a_worktree_sync() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();
    let head = ops::head_sha(&repo).unwrap();

    // Simulate the case fast_forward_to's early return exists for: HEAD is
    // already the tip, but the work tree is behind (what a merge that updated
    // only the index leaves behind).
    fs::write(root.join("README.md"), "stale\n").unwrap();
    assert_ne!(fs::read_to_string(root.join("README.md")).unwrap(), "readme\n");

    ops::fast_forward_to(&repo, &head).unwrap();

    assert_eq!(
        fs::read_to_string(root.join("README.md")).unwrap(),
        "readme\n",
        "work tree should have been brought back in line"
    );
    assert!(!ops::status(&repo).unwrap().has_changes);
}
