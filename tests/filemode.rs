//! Differential test for file modes: `ops::stage_all` + `ops::commit`
//! against `git add -A` + `git commit`, on a filesystem whose execute bit
//! disagrees with the index.
//!
//! The fixture is the iPad's: grit cloned the workspace there and left every
//! tracked file as `700`, while the trees said `644`. With
//! `core.filemode=false` -- which grit itself writes at init when it notices
//! that mismatch -- git reports nothing and commits nothing. Before this test
//! existed, the shim read the bit anyway: status listed every tracked file as
//! modified and the next commit carried 9762 mode-only changes, no content
//! changed at all.
//!
//! Both directions are pinned. The last test turns `core.filemode` back on and
//! insists the mode change IS still caught: a fix that simply stopped
//! recording the execute bit would pass the first three and be wrong.

use std::fs;
use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::ops;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "probe")
        .env("GIT_AUTHOR_EMAIL", "probe@example.com")
        .env("GIT_COMMITTER_NAME", "probe")
        .env("GIT_COMMITTER_EMAIL", "probe@example.com")
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

fn chmod(dir: &Path, path: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir.join(path), fs::Permissions::from_mode(mode)).unwrap();
}

/// A committed repository whose tracked files carry an execute bit that the
/// trees do not, with `core.filemode` set as given.
fn fixture(filemode: &str, count: usize) -> TempDir {
    let dir = TempDir::new().unwrap();
    for index in 0..count {
        fs::write(dir.path().join(format!("note-{index}.md")), format!("note {index}
")).unwrap();
    }
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/deep.md"), "deep\n").unwrap();

    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "fixture"]);
    git(dir.path(), &["config", "core.filemode", filemode]);

    // The execute bit on disk, over every tracked file. 0o700 is what the
    // iPad's checkout left behind; 0o755 is the ordinary executable case.
    for index in 0..count {
        chmod(dir.path(), &format!("note-{index}.md"), 0o700);
    }
    chmod(dir.path(), "sub/deep.md", 0o700);
    dir
}

/// Modes recorded by the commit `sha` points at, as `mode path` lines.
fn tree_modes(dir: &TempDir, sha: &str) -> Vec<String> {
    let mut lines: Vec<String> = git(dir.path(), &["ls-tree", "-r", sha])
        .lines()
        // `<mode> <type> <oid>\t<path>`, so the path is the third remaining field.
        .map(|line| {
            let mut parts = line.split_whitespace();
            let mode = parts.next().unwrap_or_default();
            let _kind = parts.next();
            let _oid = parts.next();
            format!("{mode} {}", parts.next().unwrap_or_default())
        })
        .collect();
    lines.sort();
    lines
}

#[test]
fn filemode_false_keeps_a_committed_tree_byte_identical() {
    let dir = fixture("false", 12);
    let repo = ops::open(dir.path()).unwrap();

    ops::stage_all(&repo).unwrap();
    let sha = ops::commit(&repo, "mode check", "probe", "probe@example.com").unwrap();

    // git agrees, which is the whole point: the commit changes nothing.
    assert!(
        git(dir.path(), &["diff", "--stat", "HEAD~1", "HEAD"]).trim().is_empty(),
        "the commit must not change any file, but git reports:\n{}",
        git(dir.path(), &["diff", "--stat", "HEAD~1", "HEAD"])
    );
    assert_eq!(
        tree_modes(&dir, &sha),
        tree_modes(&dir, &git(dir.path(), &["rev-parse", "HEAD~1"]).trim()),
        "every tree entry must keep the mode it was committed with"
    );
}

#[test]
fn filemode_false_reports_no_changed_files() {
    let dir = fixture("false", 12);
    let repo = ops::open(dir.path()).unwrap();

    let status = ops::status(&repo).unwrap();

    assert!(
        status.files.is_empty(),
        "an untouched checkout must not look modified, got: {:?}",
        status.files.iter().map(|f| &f.path).collect::<Vec<_>>()
    );
}

#[test]
fn filemode_false_still_reports_a_real_content_change() {
    let dir = fixture("false", 12);
    fs::write(dir.path().join("note-3.md"), "edited\n").unwrap();
    let repo = ops::open(dir.path()).unwrap();

    let status = ops::status(&repo).unwrap();
    let paths: Vec<&str> = status.files.iter().map(|f| f.path.as_str()).collect();

    assert_eq!(paths, vec!["note-3.md"], "only the edited file may appear");
    // And the commit carries the new content at the recorded mode.
    ops::stage_all(&repo).unwrap();
    let sha = ops::commit(&repo, "edit", "probe", "probe@example.com").unwrap();
    assert!(git(dir.path(), &["show", &format!("{sha}:note-3.md")]).contains("edited"));
    assert!(tree_modes(&dir, &sha).contains(&"100644 note-3.md".to_string()));
}

#[test]
fn filemode_true_still_records_an_executable_file() {
    // The other direction, so the fix cannot be "stop recording the bit".
    let dir = fixture("true", 12);
    let repo = ops::open(dir.path()).unwrap();

    ops::stage_all(&repo).unwrap();
    let sha = ops::commit(&repo, "mode check", "probe", "probe@example.com").unwrap();

    let modes = tree_modes(&dir, &sha);
    assert!(
        modes.iter().all(|line| line.starts_with("100755")),
        "with core.filemode=true the execute bit must be recorded, got: {modes:?}"
    );
    assert_eq!(
        git(dir.path(), &["diff", "--summary", "HEAD~1", "HEAD"])
            .lines()
            .filter(|line| line.contains("mode change"))
            .count(),
        13,
        "git must agree that all 13 tracked files changed mode"
    );
}

#[test]
fn an_untracked_file_lands_as_a_regular_file_when_filemode_is_false() {
    let dir = fixture("false", 3);
    chmod(dir.path(), "sub/deep.md", 0o755);
    let repo = ops::open(dir.path()).unwrap();

    // Stage the one path through the single-file entry point, which decides the
    // mode separately from stage_all.
    ops::stage_file(&repo, "sub/deep.md").unwrap();

    let index = repo.load_index().unwrap();
    let entry = index
        .entries
        .iter()
        .find(|entry| entry.path == b"sub/deep.md".as_slice())
        .expect("sub/deep.md in the index");
    assert_eq!(
        entry.mode,
        0o100644,
        "an existing entry keeps its recorded mode instead of picking up the disk bit"
    );
}

/// The repair path for a poisoned index: the mode comes from the tree, and the
/// work tree is not touched.
#[test]
fn a_mixed_reset_takes_modes_back_from_the_tree() {
    let dir = fixture("false", 4);
    let repo = ops::open(dir.path()).unwrap();

    // Stage a mode change the old way would have: write 755 into the index.
    ops::stage_all(&repo).unwrap();
    ops::commit(&repo, "poison", "probe", "probe@example.com").unwrap();
    let parent = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();

    // Something the user has not committed yet, so "work tree untouched" is testable.
    fs::write(dir.path().join("uncommitted.md"), "work in progress\n").unwrap();

    ops::reset_mixed_to(&repo, &parent).unwrap();

    let index = ops::open(dir.path()).unwrap().load_index().unwrap();
    let executable = index
        .entries
        .iter()
        .filter(|entry| entry.mode == 0o100755)
        .count();
    assert_eq!(executable, 0, "every mode must come back from the tree");

    // HEAD moved back, and the work-in-progress file is still there.
    assert_eq!(ops::head_sha(&ops::open(dir.path()).unwrap()).as_deref(), Some(parent.as_str()));
    assert!(dir.path().join("uncommitted.md").exists());

    // And the repository is clean again, rather than reporting 9763 changes.
    let status = ops::status(&ops::open(dir.path()).unwrap()).unwrap();
    assert!(
        status.files.iter().all(|f| f.path == "uncommitted.md"),
        "only the uncommitted file may remain, got: {:?}",
        status.files.iter().map(|f| &f.path).collect::<Vec<_>>()
    );
}

#[test]
fn a_mixed_reset_refuses_a_commit_that_is_not_here() {
    let dir = fixture("false", 2);
    let repo = ops::open(dir.path()).unwrap();

    let missing = "0123456789abcdef0123456789abcdef01234567";
    let err = ops::reset_mixed_to(&repo, missing).unwrap_err().to_string();

    assert!(err.contains("not in this repository"), "got: {err}");
    // HEAD must not have moved.
    assert_eq!(
        ops::head_sha(&repo).as_deref(),
        Some(git(dir.path(), &["rev-parse", "HEAD"]).trim())
    );
}
