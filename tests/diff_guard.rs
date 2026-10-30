//! The binary/oversize guard around blob → text conversion.
//!
//! The regression these pin: a commit that touched a binary blob used to be
//! read whole, lossy-UTF8'd, and fed to the histogram diff — on the device
//! that froze the process (or ended it via an uncatchable exit) instead of
//! showing a placeholder. The guard must short-circuit to an empty patch
//! flagged `binary` while plain text commits keep diffing exactly as before.

use std::fs;
use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::ops;
use neurascribe_grit_ffi::types::FileDiff;
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

/// `git init` + a first commit with one small text file, so every later
/// commit has a parent to diff against.
fn base_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    fs::write(root.join("a.txt"), b"a\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "base"]);
    dir
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn entry<'a>(diffs: &'a [FileDiff], path: &str) -> &'a FileDiff {
    diffs
        .iter()
        .find(|d| d.path == path)
        .unwrap_or_else(|| panic!("{path} is missing from the commit diff"))
}

#[test]
fn binary_file_in_commit_gets_empty_patch_flagged_binary() {
    let dir = base_repo();
    let root = dir.path();
    // A PNG magic header plus NUL bytes: the head sniff must reject it.
    fs::write(root.join("img.png"), [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 1, 2, 3]).unwrap();
    fs::write(root.join("a.txt"), b"a\nedited\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "binary + text"]);

    let repo = ops::open(root).unwrap();
    let diffs = ops::diff_commit(&repo, &head(root)).unwrap();

    let bin = entry(&diffs, "img.png");
    assert!(bin.binary, "a NUL-bearing blob must be flagged binary");
    assert_eq!(bin.patch, "", "a binary file must not produce a patch body");

    // The textual file in the same commit is unaffected: the guard must not
    // spill over into its neighbours.
    let text = entry(&diffs, "a.txt");
    assert!(!text.binary, "a text file must not be flagged binary");
    assert!(text.patch.contains("@@"), "the text patch must still have a hunk header");
}

#[test]
fn utf8_cjk_content_is_text_not_binary() {
    // Regression (2026-10-30): a mostly-Chinese markdown file measured ~60%
    // "non-text" under the old 30% ASCII-centric ratio rule and lost its diff.
    // High bytes are not evidence of binary; only NUL is.
    let dir = base_repo();
    let root = dir.path();
    let cjk: &[u8] = " UI 还是要人工设计,先列出所有想法\n- 想法一: 低成本\n- 想法二: 快速\n".as_bytes();
    assert!(cjk.iter().filter(|&&b| b >= 0x80).count() * 2 > cjk.len());
    fs::write(root.join("notes.md"), cjk).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "cjk notes"]);

    let repo = ops::open(root).unwrap();
    let diffs = ops::diff_commit(&repo, &head(root)).unwrap();

    let f = entry(&diffs, "notes.md");
    assert!(!f.binary, "UTF-8 CJK text must stay textual");
    assert!(f.patch.contains("@@"), "a CJK text file must still get a real patch");
}

#[test]
fn utf16_text_is_caught_by_the_nul_rule() {
    // UTF-16 (no BOM) writes NUL bytes at odd offsets even for pure ASCII
    // content, so the NUL-only rule still keeps it out of the text diff.
    let dir = base_repo();
    let root = dir.path();
    let utf16: Vec<u8> = "hello world"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    assert!(utf16.contains(&0));
    fs::write(root.join("wide.txt"), &utf16).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "utf16"]);

    let repo = ops::open(root).unwrap();
    let diffs = ops::diff_commit(&repo, &head(root)).unwrap();

    let f = entry(&diffs, "wide.txt");
    assert!(f.binary, "UTF-16 text has NULs and must be flagged binary");
    assert_eq!(f.patch, "");
}

#[test]
fn oversized_text_blob_is_flagged_binary_without_diffing() {
    let dir = base_repo();
    let root = dir.path();
    // Pure printable ASCII just over the 2 MiB cap: only the size rule fires.
    let body = "0123456789\n".repeat(200_001); // 2,200,011 bytes
    assert!(body.len() > 2 * 1024 * 1024);
    fs::write(root.join("big.txt"), body.as_bytes()).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "big"]);

    let repo = ops::open(root).unwrap();
    let diffs = ops::diff_commit(&repo, &head(root)).unwrap();

    let f = entry(&diffs, "big.txt");
    assert!(f.binary, "a blob over the size cap must be flagged binary");
    assert_eq!(f.patch, "", "an oversize blob must not be diffed");
}

#[test]
fn normal_text_commit_stays_textual() {
    let dir = base_repo();
    let root = dir.path();
    fs::write(root.join("a.txt"), b"a\nedited\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "edit"]);

    let repo = ops::open(root).unwrap();
    let diffs = ops::diff_commit(&repo, &head(root)).unwrap();

    assert_eq!(diffs.len(), 1, "only a.txt changed");
    let f = &diffs[0];
    assert_eq!(f.path, "a.txt");
    assert!(!f.binary, "a plain text change must not be flagged binary");
    assert!(f.patch.contains("@@"), "the patch must have a hunk header");
    assert!(f.patch.contains("+edited"), "the patch must contain the added line");
}

#[test]
fn diff_file_binary_work_tree_returns_empty_string() {
    let dir = base_repo();
    let root = dir.path();
    fs::write(root.join("a.txt"), [0x00u8, 0x01, 0x02]).unwrap();

    let repo = ops::open(root).unwrap();
    assert_eq!(ops::diff_file(&repo, "a.txt").unwrap(), "");
}

#[test]
fn diff_file_oversize_work_tree_returns_empty_string() {
    let dir = base_repo();
    let root = dir.path();
    fs::write(root.join("a.txt"), "0123456789\n".repeat(200_001)).unwrap();

    let repo = ops::open(root).unwrap();
    assert_eq!(ops::diff_file(&repo, "a.txt").unwrap(), "");
}

#[test]
fn diff_file_binary_head_text_work_tree_returns_empty_string() {
    // The guard applies per side: a text HEAD and a binary work tree must
    // still short-circuit, not diff the garbage.
    let dir = base_repo();
    let root = dir.path();
    fs::write(root.join("a.txt"), [b'a', 0, b'\n', 0, b'x', 0]).unwrap();

    let repo = ops::open(root).unwrap();
    assert_eq!(ops::diff_file(&repo, "a.txt").unwrap(), "");
}

#[test]
fn diff_file_text_change_still_diffs() {
    let dir = base_repo();
    let root = dir.path();
    fs::write(root.join("a.txt"), b"a\nedited\n").unwrap();

    let repo = ops::open(root).unwrap();
    let diff = ops::diff_file(&repo, "a.txt").unwrap();
    assert!(diff.contains("@@"), "the patch must have a hunk header");
    assert!(diff.contains("+edited"), "the patch must contain the added line");
}
