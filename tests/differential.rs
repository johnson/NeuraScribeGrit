//! Differential tests: every operation is checked against real `git` output on a
//! repository built by real git. Upstream warns grit may be "broken in ways
//! that are not exercised in the test suite", so agreement with the reference
//! implementation is the only evidence that matters here.

use std::fs;
use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::{ops, refs_ops};
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

/// The reflog has to answer "which commit was HEAD before this, who moved
/// it, when, and why" — that is the only trail left when a commit later turns
/// out to be unreadable. The old writer emitted the all-zero previous value,
/// a fabricated identity, a zero timestamp and no message, which is the exact
/// line git writes when a ref is *created*: every app commit looked like a
/// branch creation and the question was unanswerable.
#[test]
fn reflog_records_real_previous_value_identity_and_message() {
    let dir = fixture();
    let root = dir.path();
    let before = git(root, &["rev-parse", "HEAD"]).trim().to_string();

    fs::write(root.join("notes/one.md"), "line one\nline two\nline three\n").unwrap();
    let repo = ops::open(root).unwrap();
    ops::stage_all(&repo).unwrap();
    let sha = ops::commit(&repo, "sync: wip (1 files)", "NeuraScribe", "committer@neurascribe.com")
        .unwrap();

    let raw = fs::read_to_string(root.join(".git/logs/refs/heads/main")).unwrap();
    let last = raw.lines().last().expect("reflog has no entries");
    let fields: Vec<&str> = last.split_whitespace().collect();

    assert_eq!(fields[0], before, "reflog did not record the previous HEAD");
    assert_ne!(fields[0], "0".repeat(40), "previous value is the null oid, so this reads as a ref creation");
    assert_eq!(fields[1], sha, "reflog did not record the new commit");
    assert_eq!(
        fields[2..4],
        ["NeuraScribe", "<committer@neurascribe.com>"],
        "reflog did not record the real committer"
    );
    let stamp: i64 = fields[4].parse().expect("timestamp is not a number");
    assert!(stamp > 1_700_000_000, "reflog timestamp was not written: {stamp}");
    assert!(
        last.ends_with("\tcommit: sync: wip (1 files)"),
        "expected git's tab-separated message, got {last:?}"
    );
}

/// A reset moves the ref too, and it is the move most likely to strand a user
/// ("which commit was I on before?"). It must be on the record as well.
#[test]
fn hard_reset_is_recorded_in_the_reflog() {
    let dir = fixture();
    let root = dir.path();
    let first = git(root, &["rev-list", "--max-parents=0", "HEAD"]).trim().to_string();
    let second = git(root, &["rev-parse", "HEAD"]).trim().to_string();

    let repo = ops::open(root).unwrap();
    ops::reset_hard_to(&repo, &first).unwrap();

    let raw = fs::read_to_string(root.join(".git/logs/refs/heads/main")).unwrap();
    let last = raw.lines().last().expect("reflog has no entries");
    let fields: Vec<&str> = last.split_whitespace().collect();
    assert_eq!(fields[0], second, "reset did not record the commit it left");
    assert_eq!(fields[1], first, "reset did not record where it moved to");
    assert!(last.contains("reset: moving to"), "reset message missing: {last:?}");
}

/// git can read back what we wrote, and it agrees the chain is continuous.
/// Without the old value the chain is broken exactly where it matters.
#[test]
fn git_reflog_agrees_with_ours() {
    let dir = fixture();
    let root = dir.path();
    let before = git(root, &["rev-parse", "HEAD"]).trim().to_string();

    // A real content change: committing an unchanged tree with the same parent
    // and the same timestamp reproduces the existing commit object byte for
    // byte, so the ref would not move and there would be nothing to read back.
    fs::write(root.join("notes/one.md"), "line one\nline two\nplus a line\n").unwrap();
    let repo = ops::open(root).unwrap();
    ops::stage_all(&repo).unwrap();
    ops::commit(&repo, "from grit", "NeuraScribe", "committer@neurascribe.com").unwrap();

    let out = git(root, &["reflog", "show", "--pretty=%H %gs"]);
    assert!(
        out.contains("commit: from grit"),
        "git cannot read our reflog message: {out}"
    );
    // "git reflog" reads HEAD's log, not the branch's, so writing only the
    // branch log would leave this empty.
    let on_branch = git(root, &["reflog", "show", "main", "--pretty=%gs"]);
    assert!(
        on_branch.contains("commit: from grit"),
        "the branch log and HEAD's log disagree: {on_branch}"
    );
    let first = out.lines().next().expect("reflog is empty").split_whitespace().next().unwrap();
    let parent = git(root, &["rev-parse", "HEAD^"]).trim().to_string();
    assert_eq!(first, git(root, &["rev-parse", "HEAD"]).trim());
    assert_eq!(parent, before, "the commit chain does not reach the previous HEAD");
}

/// A commit the repository does not have must be named as such, before the
/// work tree or HEAD is touched — the user needs to know the *commit* is the
/// problem, and that nothing was changed.
#[test]
fn reset_to_a_missing_commit_explains_itself_and_changes_nothing() {
    let dir = fixture();
    let root = dir.path();
    let head_before = git(root, &["rev-parse", "HEAD"]).trim().to_string();
    let tree_before = fs::read_to_string(root.join("notes/one.md")).unwrap();

    let absent = "0".repeat(39) + "1";
    let repo = ops::open(root).unwrap();
    let err = ops::reset_hard_to(&repo, &absent).unwrap_err().to_string();

    assert!(err.contains("not in this repository"), "error does not say what is wrong: {err}");
    assert!(err.contains(&absent), "error does not name the commit: {err}");
    assert!(err.contains("fetched"), "error does not suggest a way out: {err}");
    assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), head_before, "HEAD moved on a failed reset");
    assert_eq!(fs::read_to_string(root.join("notes/one.md")).unwrap(), tree_before, "work tree changed on a failed reset");
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

// --- Branches ---------------------------------------------------------

#[test]
fn list_branches_matches_git() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");

    refs_ops::create_branch(&repo, "topic", None).unwrap();
    refs_ops::create_branch(&repo, "older", Some(&ops::head_sha(&repo).unwrap())).unwrap();

    let listed = refs_ops::list_branches(&repo).unwrap();
    let names: Vec<&str> = listed.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, vec!["main", "older", "topic"], "not sorted by name");

    let git_names: Vec<String> = git(root, &["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(names, git_names.iter().map(|s| s.as_str()).collect::<Vec<_>>());

    let head = listed.iter().find(|b| b.is_head).expect("a branch is HEAD");
    assert_eq!(head.name, git(root, &["branch", "--show-current"]).trim());
    assert_eq!(head.sha, git(root, &["rev-parse", "HEAD"]).trim());

    // Every listed sha must be what git resolves for the same branch.
    for branch in &listed {
        assert_eq!(
            branch.sha,
            git(root, &["rev-parse", &format!("refs/heads/{}", branch.name)]).trim(),
            "sha mismatch for {}",
            branch.name
        );
    }
}

#[test]
fn ahead_behind_matches_git() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");

    // The remote-tracking ref a real clone would have, plus tracking config
    // for BOTH branches. Ahead/behind is defined against the upstream a
    // branch tracks, so a branch with no tracking config has no counts to
    // compare -- `git status` reports the same thing.
    git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(root, &["config", "branch.main.remote", "origin"]);
    git(root, &["config", "branch.main.merge", "refs/heads/main"]);

    refs_ops::create_branch(&repo, "topic", None).unwrap();
    git(root, &["config", "branch.topic.remote", "origin"]);
    git(root, &["config", "branch.topic.merge", "refs/heads/main"]);

    // One commit on main that topic does not have.
    fs::write(root.join("notes/one.md"), "on main\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "main moves"]);
    let main_sha = git(root, &["rev-parse", "HEAD"]).trim().to_string();

    // The remote moves to that commit too, so topic is now behind it. After
    // topic's own commit below the two have genuinely diverged: one commit
    // each way, which is the state ahead/behind exists to describe.
    git(root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

    // One commit on topic that the remote does not have (topic is ahead).
    refs_ops::checkout_branch_clean(&repo, "topic").unwrap();
    fs::write(root.join("notes/sub/two.md"), "on topic\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "topic moves"]);
    refs_ops::checkout_branch_clean(&repo, "main").unwrap();

    let listed = refs_ops::list_branches(&repo).unwrap();
    let topic = listed.iter().find(|b| b.name == "topic").unwrap();
    assert_eq!(topic.upstream.as_deref(), Some("refs/remotes/origin/main"));

    let git_ahead: u32 = git(root, &["rev-list", "--count", "refs/remotes/origin/main..topic"])
        .trim()
        .parse()
        .unwrap();
    let git_behind: u32 = git(root, &["rev-list", "--count", "topic..refs/remotes/origin/main"])
        .trim()
        .parse()
        .unwrap();
    assert!(git_ahead > 0 && git_behind > 0, "fixture should diverge");
    assert_eq!(topic.ahead, git_ahead, "ahead mismatch");
    assert_eq!(topic.behind, git_behind, "behind mismatch");
    assert!(!topic.upstream_gone);

    let main_branch = listed.iter().find(|b| b.name == "main").unwrap();
    assert_eq!(main_branch.sha, main_sha);
}

#[test]
fn tracking_that_does_not_resolve_is_reported_as_gone() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");

    // Configured upstream, but nothing was ever fetched.
    git(root, &["config", "branch.main.remote", "origin"]);
    git(root, &["config", "branch.main.merge", "refs/heads/main"]);

    let listed = refs_ops::list_branches(&repo).unwrap();
    let main_branch = listed.iter().find(|b| b.name == "main").unwrap();
    assert!(main_branch.upstream.is_some());
    assert!(main_branch.upstream_gone, "missing upstream ref reported as in-sync");
    assert_eq!(main_branch.ahead, 0);
    assert_eq!(main_branch.behind, 0);
}

#[test]
fn creating_a_branch_writes_a_ref_git_can_read() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");
    let head = ops::head_sha(&repo).unwrap();

    let sha = refs_ops::create_branch(&repo, "feature/x", Some(&head)).unwrap();
    assert_eq!(sha, head);
    assert_eq!(git(root, &["rev-parse", "refs/heads/feature/x"]).trim(), head);
    assert_eq!(git(root, &["branch", "--show-current"]).trim(), "main", "HEAD moved on branch create");

    let err = refs_ops::create_branch(&repo, "feature/x", None).unwrap_err().to_string();
    assert!(err.contains("already exists"), "duplicate create was not refused: {err}");
}

#[test]
fn invalid_branch_names_are_refused_without_writing_anything() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");
    let before = refs_ops::list_branches(&repo).unwrap().len();

    for bad in ["", "  ", "a..b", "bad name", "refs/heads/dup", "-flag", "x/"] {
        let result = refs_ops::create_branch(&repo, bad, None);
        assert!(result.is_err(), "accepted an invalid branch name {bad:?}");
    }
    assert_eq!(refs_ops::list_branches(&repo).unwrap().len(), before);
}

#[test]
fn checkout_switches_branch_and_work_tree() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");

    refs_ops::create_branch(&repo, "topic", None).unwrap();
    refs_ops::checkout_branch_clean(&repo, "topic").unwrap();

    assert_eq!(ops::current_branch(&repo).unwrap(), "topic");
    assert_eq!(git(root, &["branch", "--show-current"]).trim(), "topic");
    // The commit the branch points at is the one on disk.
    assert_eq!(fs::read_to_string(root.join("notes/one.md")).unwrap(), "line one\nline two changed\n");

    let err = refs_ops::checkout_branch_clean(&repo, "nope").unwrap_err().to_string();
    assert!(err.contains("does not exist"), "missing branch was not reported: {err}");
}

#[test]
fn checkout_refuses_to_discard_uncommitted_work() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");

    refs_ops::create_branch(&repo, "topic", None).unwrap();
    refs_ops::checkout_branch_clean(&repo, "topic").unwrap();

    // Uncommitted edit, plus a commit on topic so the branches differ.
    fs::write(root.join("notes/one.md"), "work in progress\n").unwrap();
    ops::stage_all(&repo).unwrap();
    ops::commit(&repo, "topic commit", "NeuraScribe", "app@neurascribe.local").unwrap();
    refs_ops::checkout_branch_clean(&repo, "main").unwrap();
    fs::write(root.join("notes/one.md"), "uncommitted again\n").unwrap();

    let err = refs_ops::checkout_branch_clean(&repo, "topic").unwrap_err().to_string();
    assert!(err.contains("uncommitted"), "dirty switch was not refused: {err}");
    assert_eq!(ops::current_branch(&repo).unwrap(), "main", "HEAD moved despite the refusal");
    assert_eq!(fs::read_to_string(root.join("notes/one.md")).unwrap(), "uncommitted again\n");

    // Staged-but-uncommitted is the same hazard and is refused too.
    ops::stage_all(&repo).unwrap();
    assert!(refs_ops::checkout_branch_clean(&repo, "topic").is_err());
}

#[test]
fn checkout_refuses_to_overwrite_an_untracked_file() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");

    refs_ops::create_branch(&repo, "topic", None).unwrap();
    fs::write(root.join("notes/brand-new.md"), "only on topic\n").unwrap();
    ops::stage_all(&repo).unwrap();
    ops::commit(&repo, "add a file", "NeuraScribe", "app@neurascribe.local").unwrap();
    refs_ops::checkout_branch_clean(&repo, "main").unwrap();

    // A different untracked file at the same path as the branch has tracked.
    fs::write(root.join("notes/brand-new.md"), "precious, not committed\n").unwrap();
    let err = refs_ops::checkout_branch_clean(&repo, "topic").unwrap_err().to_string();
    assert!(err.contains("brand-new.md"), "collision was not reported: {err}");
    assert_eq!(fs::read_to_string(root.join("notes/brand-new.md")).unwrap(), "precious, not committed\n");
}

#[test]
fn deleting_a_branch_follows_git_rules() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");

    refs_ops::create_branch(&repo, "merged", None).unwrap();
    refs_ops::delete_branch(&repo, "merged", false).unwrap();
    assert!(refs_ops::list_branches(&repo).unwrap().iter().all(|b| b.name != "merged"));

    // Unmerged work needs force.
    refs_ops::create_branch(&repo, "unmerged", None).unwrap();
    refs_ops::checkout_branch_clean(&repo, "unmerged").unwrap();
    fs::write(root.join("notes/one.md"), "only here\n").unwrap();
    ops::stage_all(&repo).unwrap();
    let orphan = ops::commit(&repo, "orphan", "NeuraScribe", "app@neurascribe.local").unwrap();
    refs_ops::checkout_branch_clean(&repo, "main").unwrap();

    let err = refs_ops::delete_branch(&repo, "unmerged", false).unwrap_err().to_string();
    assert!(err.contains("not fully merged"), "unmerged delete was not refused: {err}");
    refs_ops::delete_branch(&repo, "unmerged", true).unwrap();
    assert!(refs_ops::list_branches(&repo).unwrap().iter().all(|b| b.name != "unmerged"));
    // The commits were only discarded from the ref, not from the object store.
    assert!(ops::resolve_ref(&repo, &orphan).is_ok());
}

#[test]
fn deleting_the_current_branch_is_refused() {
    let dir = fixture();
    let root = dir.path();
    let repo = ops::open(root).expect("grit opens the repo");

    let err = refs_ops::delete_branch(&repo, "main", true).unwrap_err().to_string();
    assert!(err.contains("current branch"), "HEAD branch delete was not refused: {err}");
    assert_eq!(ops::current_branch(&repo).unwrap(), "main");
    assert_eq!(git(root, &["rev-parse", "HEAD"]).trim(), ops::head_sha(&repo).unwrap());
}

