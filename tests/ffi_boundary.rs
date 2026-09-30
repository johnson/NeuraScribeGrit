//! What crosses the C boundary, and what must not be silently lost there.
//!
//! The credential the app hands to a network call arrives as a JSON C string.
//! It used to be parsed with `serde_json::from_str(..).ok()`, which turns *any* parse
//! failure into "no credential". The caller could not tell that apart from
//! genuinely sending nothing, so a token read out of the Keychain and then
//! mangled on the way across was reported to the user as
//!
//!     authentication failed: no credential is available for this remote
//!
//! which reads as "you never supplied a token" when the truth is "we lost the
//! one you gave us". These tests pin the distinction.

use std::ffi::{CStr, CString};
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().expect("run git");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn c(s: &str) -> CString {
    CString::new(s).expect("no interior NUL")
}

/// The message the shim left for the caller, or "" when it left none.
fn last_error() -> String {
    let raw = neurascribe_grit_ffi::grit_last_error();
    if raw.is_null() {
        return String::new();
    }
    let owned = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
    unsafe { neurascribe_grit_ffi::grit_string_free(raw) };
    owned
}

/// Seed a repository with one commit and open it through the C ABI.
///
/// A NULL callbacks/progress pointer means "the embedder has none", which the
/// shim turns into a no-progress, no-bridge call. That is enough here: every
/// case below either fails at the credential gate or on the network, so no
/// transport is ever needed.
fn open_fixture(dir: &Path) -> *mut neurascribe_grit_ffi::GritRepoHandle {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Tester"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    std::fs::write(dir.join("a.txt"), "a\n").expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "first"]);

    let path = c(dir.to_str().expect("utf8"));
    let repo = neurascribe_grit_ffi::grit_repo_open(path.as_ptr());
    assert!(!repo.is_null(), "grit_repo_open failed: {}", last_error());
    repo
}

/// Fetch with `credential` handed across the boundary, and return the message
/// the caller would see. "" means the call succeeded.
fn fetch_with_credential(dir: &Path, credential: Option<&str>) -> String {
    let repo = open_fixture(dir);
    let url = c("https://example.invalid/repo.git");
    let refspecs = c(r#"["+refs/heads/*:refs/remotes/origin/*"]"#);
    let credential = credential.map(c);

    let result = neurascribe_grit_ffi::grit_repo_fetch_json(
        repo,
        url.as_ptr(),
        refspecs.as_ptr(),
        credential.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        std::ptr::null(),
        std::ptr::null(),
    );

    let message = last_error();
    unsafe { neurascribe_grit_ffi::grit_repo_free(repo) };
    if !result.is_null() {
        unsafe { neurascribe_grit_ffi::grit_string_free(result) };
        return String::new();
    }
    message
}

/// A pointer that is present but unreadable means we lost what the caller
/// gave us. Reporting that as "no credential" blames the user for a token
/// they had in fact supplied, and sends the fix in the wrong direction.
#[test]
fn an_unreadable_credential_is_reported_as_unreadable_not_as_absent() {
    let dir = tempfile::tempdir().expect("temp");
    let message = fetch_with_credential(dir.path(), Some("{not json"));

    assert!(
        message.contains("credential JSON is unreadable"),
        "expected the credential itself to be named, got: {message}"
    );
    assert!(
        !message.contains("no credential is available"),
        "an unreadable credential must not masquerade as a missing one: {message}"
    );
}

/// The honest answer for a caller that sent nothing. Distinct from the case
/// above, and it has to stay distinct.
#[test]
fn an_absent_credential_is_not_a_credential_error() {
    let dir = tempfile::tempdir().expect("temp");
    let message = fetch_with_credential(dir.path(), None);

    assert!(
        !message.contains("credential JSON is unreadable"),
        "sending nothing is legitimate and must not be reported as a broken credential: {message}"
    );
}

/// A well-formed credential passes the gate and the failure moves on to the
/// transport. That move is the proof it was accepted rather than dropped.
#[test]
fn a_well_formed_credential_gets_past_the_gate() {
    let dir = tempfile::tempdir().expect("temp");
    let message = fetch_with_credential(dir.path(), Some(r#"{"username":"tester","password":"s3cret"}"#));

    assert!(
        !message.contains("credential JSON is unreadable"),
        "valid credential JSON was rejected at the boundary: {message}"
    );
    assert!(
        !message.contains("no credential is available"),
        "a good credential must never come back as a missing one: {message}"
    );
}

/// Absence of a host is a transport failure and must not be dressed up as an
/// authentication one: the two send the user to completely different places.
#[test]
fn a_transport_failure_is_not_reported_as_an_authentication_failure() {
    let dir = tempfile::tempdir().expect("temp");
    let message = fetch_with_credential(dir.path(), Some(r#"{"username":"tester","password":"s3cret"}"#));

    assert!(
        !message.contains("authentication"),
        "an unreachable host is not an auth problem: {message}"
    );
}

/// The contract the object-existence entry point documents: absent is an
/// answer, not an error, and carries no message.
#[test]
fn an_absent_object_reads_as_absent_and_records_no_error() {
    let dir = tempfile::tempdir().expect("temp");
    let repo = open_fixture(dir.path());
    let sha = c("0000000000000000000000000000000000000001");

    let answer = neurascribe_grit_ffi::grit_repo_has_object(repo, sha.as_ptr());
    assert_eq!(answer, 0, "a SHA the repo never had must read as absent, not as an error");
    assert_eq!(last_error(), "", "absence is an answer, so it must record no error");

    let malformed = c("not a sha");
    let answer = neurascribe_grit_ffi::grit_repo_has_object(repo, malformed.as_ptr());
    assert_eq!(answer, -1, "a malformed argument is an error");
    // The message carries the string that was rejected. A caller staring at a
    // bare "not an object id" cannot tell a truncated hash from a line of
    // prose that was never a hash, and those need different fixes.
    assert!(
        last_error().contains("not an object id") && last_error().contains("not a sha"),
        "and it must say what was rejected: {}",
        last_error()
    );

    unsafe { neurascribe_grit_ffi::grit_repo_free(repo) };
}
