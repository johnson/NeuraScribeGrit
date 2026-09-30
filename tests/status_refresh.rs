//! `ops::status` must leave behind the stat data it read.
//!
//! Status hashes every index entry whose cached stat tuple does not match
//! the file on disk. On a repository whose files were written by another
//! device -- cloned on a Mac, pulled on an iPad -- that is nearly all of
//! them, and the pass repeats forever because the index was only read, never
//! written. `git status` writes the refreshed tuples back, which is what makes
//! the second pass a stat comparison; this pins that the shim's status does
//! too, since the app calls it directly and never goes through the CLI.
//!
//! The CLI already did this and is pinned separately in grit's own harness
//! (t13421); a control run with the write-back removed fails here and passes
//! there, which is the evidence that this test is measuring the shim's path
//! rather than the CLI's.

use neurascribe_grit_ffi::ops;
use std::fs;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp");
    let root = dir.path();
    git(root, &["init", "--initial-branch=main"]);
    for (name, body) in files {
        fs::write(root.join(name), body).expect("write");
    }
    git(root, &["add", "."]);
    git(
        root,
        &["-c", "user.email=t@example.com", "-c", "user.name=T", "commit", "-m", "seed"],
    );
    dir
}

/// The mtime the index caches for `path`, re-read from the index file.
fn cached_mtime(root: &Path, path: &str) -> u32 {
    let repo = ops::open(root).expect("open");
    let index = repo.load_index().expect("load index");
    index
        .entries
        .iter()
        .find(|entry| entry.path == path.as_bytes())
        .unwrap_or_else(|| panic!("{path} is not in the index"))
        .mtime_sec
}

/// Set a file's mtime to a fixed date, so "the index is stale" does not
/// depend on the rewrite happening to land in a later second than the commit.
fn touch_to(path: &Path, stamp: &str) {
    let out = Command::new("touch")
        .arg("-t")
        .arg(stamp)
        .arg(path)
        .output()
        .expect("run touch");
    assert!(
        out.status.success(),
        "touch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn file_mtime(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("stat")
        .modified()
        .expect("mtime")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("not before the epoch")
        .as_secs() as u32
}

#[test]
fn status_writes_back_the_stat_data_it_refreshed() {
    let dir = repo_with(&[("one.txt", "one\n"), ("two.txt", "two\n")]);
    let root = dir.path();

    // Stale the index the way another device does: the content is untouched
    // and only the timestamps moved, so every entry has to be hashed and
    // nothing is actually modified. A fixed future date keeps "stale" a fact
    // rather than a race with the clock.
    touch_to(&root.join("one.txt"), "203001010101");
    touch_to(&root.join("two.txt"), "203001010101");
    let on_disk = file_mtime(&root.join("one.txt"));
    assert_ne!(
        cached_mtime(root, "one.txt"),
        on_disk,
        "setup: the index must be stale before status runs"
    );

    let repo = ops::open(root).unwrap();
    let status = ops::status(&repo).expect("status runs");
    assert!(
        status.files.is_empty(),
        "rewriting a file with the same bytes is not a change, got {:?}",
        status
    );

    assert_eq!(
        cached_mtime(root, "one.txt"),
        on_disk,
        "status read the mtime and had to hash the file; it must record what it read"
    );
}

#[test]
fn a_file_that_really_changed_keeps_its_old_stat_data() {
    let dir = repo_with(&[("note.md", "original\n")]);
    let root = dir.path();
    fs::write(root.join("note.md"), "edited\n").expect("rewrite");
    let before = cached_mtime(root, "note.md");

    let repo = ops::open(root).unwrap();
    let status = ops::status(&repo).expect("status runs");
    assert_eq!(status.files.len(), 1, "the edit has to be reported");
    assert_eq!(
        cached_mtime(root, "note.md"),
        before,
        "a changed file must keep its stale stat data, or the next status would call it clean on a timestamp alone"
    );
}
