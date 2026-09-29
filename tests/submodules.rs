//! Submodule operations, checked against real git on a real repository.
//!
//! grit has no `submodule` porcelain, so these pin the behaviour the shim
//! assembles from primitives. The oracle is `git submodule` itself: every
//! assertion about `.gitmodules`, the recorded commit, and `git status` output
//! is compared with what git does on the same repository.

use std::fs;
use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::submodules;
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn super_only() -> (TempDir, TempDir) {
    let outer = TempDir::new().unwrap();
    let sub_dir = TempDir::new().unwrap();
    let sub = sub_dir.path();
    git(sub, &["init", "-q", "-b", "main"]);
    git(sub, &["config", "user.name", "T"]);
    git(sub, &["config", "user.email", "t@t.local"]);
    fs::write(sub.join("lib.md"), "lib\n").unwrap();
    git(sub, &["add", "-A"]);
    git(sub, &["commit", "-qm", "lib"]);

    let root = outer.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@t.local"]);
    fs::write(root.join("base.md"), "base\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    (outer, sub_dir)
}

/// What `git submodule status` prints: `<sha> <path> (<describe>)`.
fn git_submodule_status(root: &Path) -> Vec<(String, String)> {
    let out = git(root, &["submodule", "status", "--cached"]);
    let mut rows: Vec<(String, String)> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?.to_string();
            let path = parts.next()?.to_string();
            Some((sha, path))
        })
        .collect();
    rows.sort();
    rows
}

#[test]
fn lists_a_submodule_git_created() {
    let (outer, sub_dir) = super_only();
    let root = outer.path();
    git(
        root,
        &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", "-b", "main", sub_dir.path().to_str().unwrap(), "vendor/lib"],
    );
    git(root, &["commit", "-qm", "add submodule"]);

    let repo = neurascribe_grit_ffi::ops::open(root).unwrap();
    let list = submodules::submodules(&repo).unwrap();

    assert_eq!(list.len(), 1, "expected one submodule");
    let only = &list[0];
    assert_eq!(only.path, "vendor/lib");
    assert!(only.initialized, "git checked it out, so it is initialized");
    assert_eq!(only.indexed_sha, Some(git(root, &["rev-parse", "HEAD:vendor/lib"]).trim().to_string()));
    assert_eq!(only.head_sha, only.indexed_sha, "a fresh clone sits on the recorded commit");
}

#[test]
fn matches_git_submodule_status() {
    let (outer, sub_dir) = super_only();
    let root = outer.path();
    for name in ["vendor/a", "vendor/b"] {
        git(
            root,
            &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", "-b", "main", sub_dir.path().to_str().unwrap(), name],
        );
    }
    git(root, &["commit", "-qm", "two"]);

    let repo = neurascribe_grit_ffi::ops::open(root).unwrap();
    let mut mine: Vec<(String, String)> = submodules::submodules(&repo)
        .unwrap()
        .iter()
        .map(|s| (s.indexed_sha.clone().unwrap_or_default(), s.path.clone()))
        .collect();
    mine.sort();
    assert_eq!(mine, git_submodule_status(root), "differs from git submodule status");
}

#[test]
fn empty_when_there_is_no_gitmodules() {
    let (outer, _) = super_only();
    let repo = neurascribe_grit_ffi::ops::open(outer.path()).unwrap();
    assert!(submodules::submodules(&repo).unwrap().is_empty());
}

#[test]
fn deinitialized_submodule_reports_not_initialized() {
    let (outer, sub_dir) = super_only();
    let root = outer.path();
    git(
        root,
        &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", "-b", "main", sub_dir.path().to_str().unwrap(), "vendor/lib"],
    );
    git(root, &["commit", "-qm", "add"]);
    git(root, &["submodule", "deinit", "-f", "vendor/lib"]);

    let repo = neurascribe_grit_ffi::ops::open(root).unwrap();
    let list = submodules::submodules(&repo).unwrap();
    assert_eq!(list.len(), 1);
    assert!(!list[0].initialized, "deinit emptied the work tree");
    assert!(list[0].indexed_sha.is_some(), "the superproject still records the commit");
}
#[test]
fn record_submodule_matches_what_git_submodule_add_writes() {
    let (outer, sub_dir) = super_only();
    let root = outer.path();
    let sub_url = sub_dir.path().to_str().unwrap().to_string();

    // git's version, as the oracle: a real clone plus a real .gitmodules.
    git(
        root,
        &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", "-b", "main", &sub_url, "vendor/lib"],
    );
    git(root, &["commit", "-qm", "add"]);
    let expected_head = git(root, &["rev-parse", "HEAD:vendor/lib"]).trim().to_string();

    // Ours, on a fresh repo: a bare `git clone` for the work tree, then
    // record_submodule for the registration and the gitlink.
    let (other, _) = super_only();
    fs::create_dir_all(other.path().join("vendor")).unwrap();
    git(other.path(), &["-c", "protocol.file.allow=always", "clone", "-q", "-b", "main", &sub_url, "vendor/lib"]);

    let head = git(&other.path().join("vendor/lib"), &["rev-parse", "HEAD"]);
    let head = head.trim().to_string();
    let repo = neurascribe_grit_ffi::ops::open(other.path()).unwrap();
    submodules::record_submodule(&repo, "vendor/lib", "vendor/lib", &sub_url, &head).unwrap();

    // The registered commit and the section must be what git sees.
    assert_eq!(submodules::indexed_sha(&repo, "vendor/lib").unwrap(), expected_head);
    assert_eq!(submodules::name_for_path(&repo, "vendor/lib").unwrap(), "vendor/lib");
}

#[test]
fn recorded_submodule_commits_like_git_made_it() {
    let (_oracle, sub_dir) = super_only();
    let sub_url = sub_dir.path().to_str().unwrap().to_string();

    let (other, _) = super_only();
    fs::create_dir_all(other.path().join("vendor")).unwrap();
    git(other.path(), &["-c", "protocol.file.allow=always", "clone", "-q", "-b", "main", &sub_url, "vendor/lib"]);
    let head = git(&other.path().join("vendor/lib"), &["rev-parse", "HEAD"]);
    let head = head.trim().to_string();
    let repo = neurascribe_grit_ffi::ops::open(other.path()).unwrap();
    submodules::record_submodule(&repo, "lib", "vendor/lib", &sub_url, &head).unwrap();

    // git must accept the result: same gitlink shape as `submodule add`
    // produced on the oracle repo.
    let ours = git(other.path(), &["ls-files", "-s", "vendor/lib"]);
    assert!(ours.starts_with("160000 "), "expected a gitlink, got: {ours}");
    // ls-files -s prints: <mode> <sha> <stage>
    let fields: Vec<&str> = ours.split_whitespace().collect();
    assert_eq!(fields[1], head, "the gitlink names the clone's commit");
    assert_eq!(fields[2], "0", "a freshly added submodule is at stage 0");
    assert_eq!(
        git(other.path(), &["status", "--porcelain"]),
        "A  .gitmodules\nA  vendor/lib\n",
        "only the registration and the gitlink should be staged",
    );
}

#[test]
fn checkout_moves_the_submodule_to_the_recorded_commit() {
    let (outer, sub_dir) = super_only();
    let root = outer.path();
    let sub_url = sub_dir.path().to_str().unwrap().to_string();
    git(
        root,
        &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", "-b", "main", &sub_url, "vendor/lib"],
    );
    git(root, &["commit", "-qm", "add"]);
    let recorded = git(root, &["rev-parse", "HEAD:vendor/lib"]).trim().to_string();

    // Move the submodule off the recorded commit, the way an accidental
    // checkout inside the submodule would.
    fs::write(sub_dir.path().join("lib.md"), "lib v2\n").unwrap();
    git(sub_dir.path(), &["commit", "-aqm", "v2"]);
    git(&root.join("vendor/lib"), &["checkout", "-q", recorded.as_str()]);

    let repo = neurascribe_grit_ffi::ops::open(root).unwrap();
    let moved = submodules::checkout_submodule_to(&repo, "vendor/lib", &recorded).unwrap();
    assert_eq!(moved, recorded);
    assert_eq!(
        git(&root.join("vendor/lib"), &["rev-parse", "HEAD"]).trim(),
        recorded,
    );
}

#[test]
fn remove_clears_registration_gitlink_worktree_and_repository() {
    let (outer, sub_dir) = super_only();
    let root = outer.path();
    let sub_url = sub_dir.path().to_str().unwrap().to_string();
    git(
        root,
        &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", "-b", "main", &sub_url, "vendor/lib"],
    );
    git(root, &["commit", "-qm", "add"]);

    let repo = neurascribe_grit_ffi::ops::open(root).unwrap();
    submodules::remove_submodule(&repo, "vendor/lib").unwrap();

    assert!(submodules::submodules(&repo).unwrap().is_empty());
    assert!(!root.join(".gitmodules").exists(), "the file goes when the last one leaves");
    assert!(!root.join("vendor/lib").exists(), "work tree removed");
    assert!(
        !root.join(".git/modules/vendor/lib").exists(),
        "the submodule's own repository is removed too",
    );
    assert_eq!(submodules::indexed_sha(&repo, "vendor/lib").unwrap_err().to_string(), "vendor/lib is not a registered submodule");
}

#[test]
fn removing_one_keeps_the_other() {
    let (outer, sub_dir) = super_only();
    let root = outer.path();
    let sub_url = sub_dir.path().to_str().unwrap().to_string();
    for name in ["vendor/a", "vendor/b"] {
        git(
            root,
            &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", "-b", "main", &sub_url, name],
        );
    }
    git(root, &["commit", "-qm", "two"]);

    let repo = neurascribe_grit_ffi::ops::open(root).unwrap();
    submodules::remove_submodule(&repo, "vendor/a").unwrap();

    let left = submodules::submodules(&repo).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].path, "vendor/b");
    assert!(root.join(".gitmodules").exists(), "one submodule is still registered");
}
