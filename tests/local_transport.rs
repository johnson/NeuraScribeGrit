//! A local remote must not be pushed through the HTTP bridge.
//!
//! The Swift side answers HTTP with URLSession, which has no `file://`
//! transport, so a local fetch routed through the bridge fails before a single
//! byte moves -- the caller just sees `.lastFailed`. grit already has a local
//! transport; these tests pin that we take it.

use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::{http::HttpCallbacks, net, ops, progress::ProgressCallbacks};

/// The host build uses grit's own ureq client, so the embedder hands over no
/// Swift callbacks. Kept as a helper so each call site stays readable.
fn no_progress() -> ProgressCallbacks {
    ProgressCallbacks::none()
}

/// A local remote never reaches the HTTP bridge, so this is what the call
/// site passes to prove it.
fn no_http() -> HttpCallbacks {
    HttpCallbacks::none()
}

const REFS: &str = "refs/heads/*:refs/remotes/origin/*";

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository acting as the remote, with one commit on `main`.
fn remote(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("remote.git");
    std::fs::create_dir_all(&path).expect("mkdir");
    git(&path, &["init", "--bare", "--initial-branch=main"]);

    // A bare repo has no work tree to commit from, so seed it through one.
    let seed = dir.join("seed");
    std::fs::create_dir_all(&seed).expect("mkdir seed");
    std::fs::write(seed.join("shared.md"), "base\n").expect("write");
    git(&seed, &["init", "--initial-branch=main"]);
    git(&seed, &["add", "."]);
    git(&seed, &["-c", "user.email=t@example.com", "-c", "user.name=T", "commit", "-m", "seed"]);
    git(&seed, &["push", path.to_str().expect("utf8"), "main"]);
    path
}

fn empty_local(dir: &Path) -> std::path::PathBuf {
    let local = dir.join("local");
    std::fs::create_dir_all(&local).expect("mkdir local");
    // grit only opens an existing repository, so start one the way the app does.
    git(&local, &["init", "--initial-branch=main"]);
    let _repo = ops::open(&local).expect("open");
    local.join(".git")
}

#[test]
fn plain_path_remote_fetches_without_the_http_bridge() {
    let dir = tempfile::tempdir().expect("temp");
    let remote = remote(dir.path());
    let local = empty_local(dir.path());

    let result = net::fetch(
        local.as_path(),
        remote.to_str().expect("utf8"),
        &[REFS.to_string()],
        None,
        no_http(),
        no_progress(),
    )
    .expect("a local remote must not need the HTTP bridge");

    let key = "refs/remotes/origin/main";
    assert!(
        result.updated_refs.contains_key(key),
        "expected {key} among {:?}",
        result.updated_refs
    );
}

#[test]
fn file_url_remote_fetches_too() {
    let dir = tempfile::tempdir().expect("temp");
    let remote = remote(dir.path());
    let local = empty_local(dir.path());

    let url = format!("file://{}", remote.display());
    let result = net::fetch(local.as_path(), &url, &[REFS.to_string()], None, no_http(), no_progress())
        .expect("a file:// remote is local");
    assert!(result.updated_refs.contains_key("refs/remotes/origin/main"));
}

#[test]
fn an_https_remote_is_not_treated_as_local() {
    // Guards the opposite mistake: treating any `scheme://` string as a path
    // would make a bad remote URL read a nonsense directory instead of
    // reporting the transport error, which is far harder to diagnose.
    let dir = tempfile::tempdir().expect("temp");
    let local = empty_local(dir.path());

    let err = net::fetch(
        local.as_path(),
        "https://example.invalid/repo.git",
        &[REFS.to_string()],
        None,
        no_http(),
        no_progress(),
    )
    .expect_err("no bridge is installed, so this cannot succeed");
    let message = format!("{err:#}");
    assert!(
        !message.contains("could not find repository at"),
        "the local path was used for an https remote: {message}"
    );
}
