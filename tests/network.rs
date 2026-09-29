//! Clone / fetch / push compared against real `git`, over a real HTTP server.
//!
//! The server is `tests/git_http_server.py`, a thin CGI wrapper around
//! `git http-backend`. Running the same operation through grit and through the
//! `git` binary, then comparing the results, is the only way to claim the
//! wire protocol actually works -- grit's own tests run against grit's own
//! server, which proves nothing about interoperability with a real one.

use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::{net, ops, types::Credential};

const USER: &str = "tester";
const PASS: &str = "s3cret";

fn credential() -> Credential {
    Credential { username: USER.to_string(), password: PASS.to_string() }
}

fn url() -> String {
    std::env::var("NSG_TEST_GIT_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8099/upstream.git".to_string())
}

fn base64(input: &str) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.as_bytes().chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// True when the test server is reachable, so the suite stays runnable
/// without it instead of failing for an unrelated reason.
fn server_up() -> bool {
    let spec = format!("{}/info/refs?service=git-upload-pack", net::http_base_url(&url()));
    let auth = base64(&format!("{USER}:{PASS}"));
    Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-H", &format!("Authorization: Basic {auth}"), &spec])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|code| code.trim() == "200")
        .unwrap_or(false)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The same remote with the credentials inline, which is the only way to stop
/// the real `git` from trying to prompt on a terminal that does not exist.
fn url_with_auth() -> String {
    url().replacen("http://", &format!("http://{USER}:{PASS}@"), 1)
}

fn clone_with_git(dest: &Path) {
    let out = Command::new("git")
        .args(["clone", &url_with_auth(), dest.to_str().expect("path")])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every path in a checked-out work tree, relative and sorted.
fn work_tree_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("readable directory") {
            let entry = entry.expect("readable entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace(char::from(92), "/"));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn clone_produces_the_same_tree_as_git() {
    if !server_up() {
        eprintln!("skipping: test git server is not reachable");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let ours = dir.path().join("grit");
    let theirs = dir.path().join("git");

    let head = net::clone(&url(), &ours, None, Some(&credential())).expect("grit clone");
    clone_with_git(&theirs);

    assert_eq!(
        work_tree_files(&ours),
        work_tree_files(&theirs),
        "clone should materialise the same files as git"
    );
    assert_eq!(
        git(&theirs, &["rev-parse", "HEAD"]),
        head,
        "grit's clone should check out the same commit git does"
    );
    // A clone that lands on a detached HEAD cannot be pushed from or pulled
    // without the user noticing, so HEAD must be on a branch.
    assert_eq!(
        ops::current_branch(&ops::open(&ours).expect("open clone")).expect("branch"),
        git(&theirs, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "clone should leave HEAD on the default branch"
    );
}

#[test]
fn fetch_reports_the_same_refs_git_does() {
    if !server_up() {
        eprintln!("skipping: test git server is not reachable");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let ours = dir.path().join("grit");
    let theirs = dir.path().join("git");

    net::clone(&url(), &ours, None, Some(&credential())).expect("grit clone");
    clone_with_git(&theirs);

    let result = net::fetch(
        &ours.join(".git"),
        &url(),
        &["refs/heads/*:refs/remotes/origin/*".to_string()],
        Some(&credential()),
    )
    .expect("grit fetch");
    let ours_tip = git(&ours, &["rev-parse", "HEAD"]);

    assert!(
        result.updated_refs.values().any(|oid| *oid == ours_tip),
        "fetch should report the remote tip, got {:?}",
        result.updated_refs
    );
    assert_eq!(
        result.default_branch.as_deref(),
        Some("main"),
        "the server advertises its default branch"
    );
    assert_eq!(
        git(&ours, &["rev-parse", "HEAD"]),
        git(&theirs, &["rev-parse", "HEAD"]),
        "a fresh fetch should leave grit where git leaves it"
    );
}

#[test]
fn push_lands_the_commit_git_would_land() {
    if !server_up() {
        eprintln!("skipping: test git server is not reachable");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let ours = dir.path().join("grit");
    let theirs = dir.path().join("git");

    // Two independent clones, so the push below is a real ref update.
    net::clone(&url(), &ours, None, Some(&credential())).expect("grit clone");
    clone_with_git(&theirs);

    std::fs::write(ours.join("pushed.md"), "hello from grit").expect("write file");
    let repo = ops::open(&ours).expect("open clone");
    ops::stage_all(&repo).expect("stage");
    let sha = ops::commit(&repo, "pushed by the grit shim", "Test", "test@example.com")
        .expect("commit");

    let result = net::push(&ours.join(".git"), &url(), "main", false, Some(&credential()))
        .expect("grit push");

    assert_eq!(result.refs.len(), 1, "one ref was pushed");
    assert_eq!(result.refs[0].status, "ok", "push should be accepted");
    assert_eq!(result.refs[0].name, "refs/heads/main");

    // The real check: another client fetches and sees the new commit.
    git(&theirs, &["fetch", "origin"]);
    assert_eq!(
        git(&theirs, &["rev-parse", "origin/main"]),
        sha,
        "the pushed commit should be visible to another git client"
    );
    assert_eq!(
        git(&theirs, &["show", "origin/main:pushed.md"]),
        "hello from grit",
        "the pushed file should come back byte-for-byte"
    );
}

#[test]
fn push_reports_up_to_date_rather_than_failing() {
    if !server_up() {
        eprintln!("skipping: test git server is not reachable");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let ours = dir.path().join("grit");
    net::clone(&url(), &ours, None, Some(&credential())).expect("grit clone");

    let result = net::push(&ours.join(".git"), &url(), "main", false, Some(&credential()))
        .expect("a no-op push should succeed");
    assert_eq!(
        result.refs[0].status, "up_to_date",
        "pushing the current tip should be up to date, got {:?}",
        result.refs[0]
    );
}

#[test]
fn bad_credentials_fail_with_an_auth_error_not_a_hang() {
    if !server_up() {
        eprintln!("skipping: test git server is not reachable");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let bad = Credential {
        username: USER.to_string(),
        password: "definitely-not-the-password".to_string(),
    };
    let err = net::clone(&url(), &dir.path().join("nope"), None, Some(&bad))
        .expect_err("a wrong password must not succeed");
    let text = format!("{err:#}").to_lowercase();
    assert!(
        text.contains("credential") || text.contains("auth") || text.contains("401"),
        "expected an auth failure, got: {err:#}"
    );
}

#[test]
fn url_normalisation_matches_git_spellings() {
    assert_eq!(
        net::http_base_url("http://127.0.0.1:8099/upstream.git"),
        "http://127.0.0.1:8099/upstream"
    );
    assert_eq!(
        net::http_base_url("http://127.0.0.1:8099/upstream.git/"),
        "http://127.0.0.1:8099/upstream"
    );
    // scp-style without a scheme becomes http, the way git does.
    assert_eq!(
        net::http_base_url("tester@127.0.0.1:upstream.git"),
        "http://tester@127.0.0.1/upstream"
    );
}
