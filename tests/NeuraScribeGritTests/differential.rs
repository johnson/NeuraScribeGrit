//! Differential tests: every operation is checked against real `git` output on a
//! repository built by real git. Upstream warns grit may be "broken in ways
//! that are not exercised in the test suite", so agreement with the reference
//! implementation is the only evidence that matters here.

use std::fs;
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

/// A repo with two commits, a subdirectory, an executable file, a .gitignore
/// and a configured remote — the shapes the app actually meets.
fn fixture() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tester"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["remote", "add", "origin", "https://example.com/acme/notes.git"]);

    fs::create_dir_all(root.join("notes/sub")).unwrap();
    fs::write(root.join("notes/one.md"), "line one\nline two\n").unwrap();
    fs::write(root.join("notes/sub/two.md"), "alpha\nbeta\n").unwrap();
    fs::write(root.join(".gitignore"), "build/\n*.log\n").unwrap();
    fs::write(root.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "first"]);

    fs::write(root.join("notes/one.md"), "line one\nline two changed\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "second"]);

    dir
}

#[test]
fn open_and_identity_match_git() {
    let dir = fixture();
    let repo = ops::open(dir.path()).expect("grit opens the repo");

    assert!(ops::exists(dir.path()));
    assert_eq!(ops::current_branch(&repo).unwrap(), "main");
    assert_eq!(
        ops::current_branch(&repo).unwrap(),
        git(dir.path(), &["branch", "--show-current"]).trim()
    );
    assert_eq!(
        ops::head_sha(&repo).unwrap(),
        git(dir.path(), &["rev-parse", "HEAD"]).trim()
    );
}

#[test]
fn list_remotes_matches_git() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();
    let remotes = ops::list_remotes(&repo).unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].name, "origin");
    assert_eq!(
        remotes[0].url,
        git(dir.path(), &["remote", "get-url", "origin"]).trim()
    );
}

#[test]
fn status_of_clean_tree_is_empty() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();
    let status = ops::status(&repo).unwrap();
    assert!(!status.has_changes, "clean tree reported changes: {:?}", status.files);
    assert_eq!(status.branch, "main");
}

#[test]
fn status_matches_git_porcelain() {
    let dir = fixture();
    let root = dir.path();

    // Modification, addition, deletion, an untracked directory with two files,
    // plus an ignored dir and an ignored file that must NOT appear.
    fs::write(root.join("notes/one.md"), "line one\nline two changed again\n").unwrap();
    fs::write(root.join("notes/new.md"), "brand new\n").unwrap();
    fs::remove_file(root.join("notes/sub/two.md")).unwrap();
    fs::create_dir_all(root.join("fresh")).unwrap();
    fs::write(root.join("fresh/a.txt"), "a\n").unwrap();
    fs::write(root.join("fresh/b.txt"), "b\n").unwrap();
    fs::create_dir_all(root.join("build")).unwrap();
    fs::write(root.join("build/out.o"), "noise\n").unwrap();
    fs::write(root.join("debug.log"), "noise\n").unwrap();

    let status = ops::status(&ops::open(root).unwrap()).unwrap();
    let got: Vec<(String, String)> =
        status.files.iter().map(|f| (f.path.clone(), f.status.clone())).collect();

    let mut expected: Vec<(String, String)> = Vec::new();
    for line in git(root, &["status", "--porcelain", "-uall"]).lines() {
        if line.len() < 4 {
            continue;
        }
        // Porcelain v1 is two columns: X = staged, Y = work tree. The app shows
        // one status per path, preferring the staged one when both are set.
        let (code, path) = line.split_at(3);
        let mut chars = code.chars();
        let staged = chars.next().unwrap_or(' ');
        let unstaged = chars.next().unwrap_or(' ');
        let pick = |c: char| match c {
            'M' => "modified",
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'T' => "type_changed",
            'U' => "unmerged",
            _ => "?",
        };
        let status = if staged == '?' {
            "untracked"
        } else if staged != ' ' {
            pick(staged)
        } else {
            pick(unstaged)
        };
        expected.push((path.to_string(), status.to_string()));
    }
    expected.sort();

    assert_eq!(got, expected, "grit status diverged from git");
    assert!(
        !got.iter().any(|(p, _)| p.ends_with("debug.log")),
        "ignored .log leaked into status"
    );
    assert!(
        !got.iter().any(|(p, _)| p.starts_with("build/")),
        "ignored build/ leaked into status"
    );
}

#[test]
fn stage_all_then_commit_matches_git_tree() {
    let dir = fixture();
    let root = dir.path();
    fs::write(root.join("notes/one.md"), "changed by grit\n").unwrap();
    fs::write(root.join("notes/added.md"), "new by grit\n").unwrap();
    fs::remove_file(root.join("notes/sub/two.md")).unwrap();

    let repo = ops::open(root).unwrap();
    ops::stage_all(&repo).unwrap();

    let mut a: Vec<String> =
        ops::staged_entries(&repo).unwrap().iter().map(|f| f.path.clone()).collect();
    let mut b: Vec<String> = git(root, &["diff", "--cached", "--name-only"])
        .lines()
        .map(str::to_string)
        .collect();
    a.sort();
    b.sort();
    if a != b {
        panic!(
            "grit stage_all staged a different set than git\ngrit={a:?}\ngit={b:?}\n\
             ls-files={}\nstatus={:?}\nls-tree={}",
            git(root, &["ls-files", "-s"]),
            git(root, &["status", "--porcelain"]),
            git(root, &["ls-tree", "-r", "HEAD"]),
        );
    }

    let sha = ops::commit(&repo, "grit commit", "NeuraScribe", "app@neurascribe.local").unwrap();
    assert_eq!(sha, git(root, &["rev-parse", "HEAD"]).trim(), "commit sha differs from git");
    assert_eq!(git(root, &["log", "-1", "--pretty=%s"]).trim(), "grit commit");
    assert!(!ops::status(&ops::open(root).unwrap()).unwrap().has_changes);
}

#[test]
fn diff_file_matches_git() {
    let dir = fixture();
    let root = dir.path();
    fs::write(root.join("notes/one.md"), "line one\nline two changed\nline three\n").unwrap();

    let repo = ops::open(root).unwrap();
    let grit_diff = ops::diff_file(&repo, "notes/one.md").unwrap();
    let git_diff = git(root, &["diff", "--", "notes/one.md"]);

    // Compare hunks and +/- content; the ---/+++ headers may format prefixes
    // differently without that being a defect.
    let norm = |s: &str| -> Vec<String> {
        s.lines()
            .filter(|l| l.starts_with('+') || l.starts_with('-') || l.starts_with('@'))
            .map(|l| l.trim_end().to_string())
            .collect()
    };
    assert_eq!(norm(&grit_diff), norm(&git_diff), "grit diff body differs from git");
    assert!(grit_diff.contains("@@"), "grit diff has no hunk header");
}

#[test]
fn changed_files_and_commit_diff_match_git() {
    let dir = fixture();
    let root = dir.path();
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let first = git(root, &["rev-list", "--max-parents=0", "HEAD"]).trim().to_string();
    let repo = ops::open(root).unwrap();

    let changed: Vec<String> = ops::changed_files(&repo, &first, &head)
        .unwrap()
        .iter()
        .map(|f| f.path.clone())
        .collect();
    let git_changed: Vec<String> =
        git(root, &["diff", "--name-only", &first, &head]).lines().map(str::to_string).collect();
    assert_eq!(changed, git_changed, "changed_files diverged from git");

    let patches = ops::diff_commit(&repo, &head).unwrap();
    let touched: Vec<String> = patches.iter().map(|d| d.path.clone()).collect();
    assert_eq!(touched, git_changed, "diff_commit touched a different file set");
    assert!(patches[0].patch.contains("@@"), "commit patch has no hunk header");
}

#[test]
fn ancestry_primitives_match_git() {
    let dir = fixture();
    let root = dir.path();
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let first = git(root, &["rev-list", "--max-parents=0", "HEAD"]).trim().to_string();
    let repo = ops::open(root).unwrap();

    assert!(ops::is_ancestor(&repo, &first, &head).unwrap());
    assert!(!ops::is_ancestor(&repo, &head, &first).unwrap());
    assert_eq!(ops::merge_base(&repo, &first, &head).unwrap().as_deref(), Some(first.as_str()));

    assert_eq!(ops::ahead_count(&repo, &first).unwrap(), 1);
    assert_eq!(ops::ahead_count(&repo, &head).unwrap(), 0);
}

#[test]
fn blob_text_matches_git_show() {
    let dir = fixture();
    let root = dir.path();
    let head = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let repo = ops::open(root).unwrap();

    assert_eq!(
        ops::blob_text(&repo, &head, "notes/one.md").unwrap().as_deref(),
        Some("line one\nline two changed\n")
    );
    assert_eq!(
        ops::blob_text(&repo, &head, "notes/sub/two.md").unwrap().as_deref(),
        Some("alpha\nbeta\n")
    );
    assert!(ops::blob_text(&repo, &head, "nope.md").unwrap().is_none());
}

#[test]
fn reset_file_to_head_restores_content() {
    let dir = fixture();
    let root = dir.path();
    fs::write(root.join("notes/one.md"), "garbage\n").unwrap();

    let repo = ops::open(root).unwrap();
    ops::reset_file_to_head(&repo, "notes/one.md").unwrap();
    assert_eq!(
        fs::read_to_string(root.join("notes/one.md")).unwrap(),
        "line one\nline two changed\n"
    );
    assert!(
        !ops::status(&ops::open(root).unwrap()).unwrap().has_changes,
        "reset left the tree dirty"
    );
}

#[test]
fn hard_reset_restores_previous_commit() {
    let dir = fixture();
    let root = dir.path();
    let first = git(root, &["rev-list", "--max-parents=0", "HEAD"]).trim().to_string();

    fs::write(root.join("scratch.md"), "temp\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "third"]);
    assert!(root.join("scratch.md").exists());

    let repo = ops::open(root).unwrap();
    let landed = ops::reset_hard_to(&repo, &first).unwrap();
    assert_eq!(landed, first);
    assert!(!root.join("scratch.md").exists(), "hard reset left a deleted file behind");
    assert_eq!(
        fs::read_to_string(root.join("notes/one.md")).unwrap(),
        "line one\nline two\n",
        "hard reset did not restore the older content"
    );
    assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), first, "branch ref did not move");
}

#[test]
fn hard_reset_removes_untracked_file_added_later() {
    let dir = fixture();
    let root = dir.path();
    let first = git(root, &["rev-list", "--max-parents=0", "HEAD"]).trim().to_string();

    fs::write(root.join("only-in-later.md"), "later\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "adds a file"]);

    let repo = ops::open(root).unwrap();
    ops::reset_hard_to(&repo, &first).unwrap();
    assert!(
        !root.join("only-in-later.md").exists(),
        "file absent from the target commit survived reset"
    );
}

#[test]
fn executable_bit_survives_status_and_commit() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).unwrap();
    assert!(
        !ops::status(&repo).unwrap().files.iter().any(|f| f.path == "run.sh"),
        "exec bit counted as a change"
    );

    ops::stage_all(&repo).unwrap();
    let sha = ops::commit(&repo, "noop", "NeuraScribe", "app@neurascribe.local").unwrap();
    let mode = git(root, &["ls-tree", &sha, "run.sh"]).split(' ').next().unwrap_or("").to_string();
    assert_eq!(mode, "100755", "executable mode was lost");
}
