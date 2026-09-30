//! C-ABI surface for NeuraScribe, so the Swift app can drive grit-lib.
//!
//! Conventions:
//! * every fallible call returns NULL (or a negative int) and leaves the reason
//!   in a thread-local slot readable through `grit_last_error`;
//! * strings returned as `*mut c_char` are owned by the caller and must be
//!   released with `grit_string_free`;
//! * structured results are JSON, which keeps the ABI small and stable while
//!   the Swift side decodes into its own Codable types.

pub mod http;
pub mod index_ops;
pub mod net;
pub mod ops;
pub mod progress;
pub mod submodules;
pub mod types;

use std::cell::RefCell;
use std::ffi::{c_char, CStr, CString};
use std::path::Path;

use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

/// The opaque repository handle, under the name the C header gives it.
///
/// Every `grit_repo_*` entry point takes and returns `*mut Repository`, but
/// `use` alone does not make the type nameable from outside the crate, so Rust callers
/// (and tests) could not spell a handle's type at all. Re-exporting it under
/// the header's own name keeps the two sides reading the same.
pub use grit_lib::repo::Repository as GritRepoHandle;

thread_local! {
    static LAST_ERROR: RefCell<CString> =
        RefCell::new(CString::new("").expect("empty string is valid"));
}

fn set_error(message: &str) {
    let sanitized: String = message.chars().filter(|c| *c != '\0').take(4096).collect();
    let value =
        CString::new(sanitized).unwrap_or_else(|_| CString::new("error").expect("static literal"));
    LAST_ERROR.with(|slot| *slot.borrow_mut() = value);
}

/// Forget whatever the previous call on this thread recorded.
///
/// Callers read the slot to tell a real failure from a NULL that means "nothing
/// to report" (merge_base on unrelated commits does exactly that), so a message
/// left over from an earlier failure would otherwise be attributed to a call
/// that never set one. Every guarded entry point clears before it runs.
fn clear_error() {
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new("").expect("empty string is valid");
    });
}

/// Last error recorded on the calling thread. Never NULL; empty when no error
/// has been recorded. Caller frees with `grit_string_free`.
#[no_mangle]
pub extern "C" fn grit_last_error() -> *mut c_char {
    LAST_ERROR.with(|slot| slot.borrow().clone().into_raw())
}

/// Free a string returned by this library.
#[no_mangle]
pub unsafe extern "C" fn grit_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

fn out_string(value: String) -> *mut c_char {
    match CString::new(value) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            set_error("string contained an interior NUL");
            std::ptr::null_mut()
        }
    }
}

/// Borrow a C string argument as an owned `String`.
unsafe fn take_str(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok().map(str::to_string)
}

fn guard_string<F: FnOnce() -> anyhow::Result<String>>(f: F) -> *mut c_char {
    clear_error();
    match f() {
        Ok(value) => out_string(value),
        Err(err) => {
            set_error(&format!("{err:#}"));
            std::ptr::null_mut()
        }
    }
}

fn guard_json<F: FnOnce() -> anyhow::Result<serde_json::Value>>(f: F) -> *mut c_char {
    guard_string(move || match f() {
        Ok(value) => serde_json::to_string(&value).map_err(anyhow::Error::from),
        Err(err) => Err(err),
    })
}

fn guard_unit<F: FnOnce() -> anyhow::Result<()>>(f: F) -> i32 {
    clear_error();
    match f() {
        Ok(()) => 0,
        Err(err) => {
            set_error(&format!("{err:#}"));
            -1
        }
    }
}

/// Whether the repository can still read the object named by `sha`.
///
/// Returns 1 for present, 0 for absent, -1 when the argument is not a SHA.
/// Absence is an ordinary answer, not an error, so nothing is recorded in the
/// last-error slot: a caller asking "is this commit still here?" should not
/// have to distinguish "no" from "I could not tell".
///
/// The app needs this because its history list can outlive the objects behind
/// it. A row is rendered from a commit hash captured earlier; if that commit is
/// pruned, or was made on another device and never fetched here, every action
/// on the row fails deep inside the odb with a bare hash and no explanation.
#[no_mangle]
pub extern "C" fn grit_repo_has_object(repo: *mut Repository, sha: *const c_char) -> i32 {
    clear_error();
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    let Some(sha) = (unsafe { take_str(sha) }) else {
        set_error("null sha");
        return -1;
    };
    let Ok(oid) = sha.parse::<ObjectId>() else {
        set_error(&format!("not an object id: {sha:?}"));
        return -1;
    };
    i32::from(repo.odb.exists(&oid))
}

/// Open a repository. Returns NULL when `path` is not inside a work tree.
#[no_mangle]
pub extern "C" fn grit_repo_open(path: *const c_char) -> *mut Repository {
    let Some(path) = (unsafe { take_str(path) }) else {
        set_error("null path");
        return std::ptr::null_mut();
    };
    clear_error();
    match ops::open(Path::new(&path)) {
        Ok(repo) => Box::into_raw(Box::new(repo)),
        Err(err) => {
            set_error(&format!("{err:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Release a handle from `grit_repo_open`.
#[no_mangle]
pub unsafe extern "C" fn grit_repo_free(repo: *mut Repository) {
    if !repo.is_null() {
        drop(Box::from_raw(repo));
    }
}

/// 1 when `path` is inside a git work tree, 0 otherwise. Never errors.
#[no_mangle]
pub extern "C" fn grit_repo_exists(path: *const c_char) -> i32 {
    match unsafe { take_str(path) } {
        Some(path) if ops::exists(Path::new(&path)) => 1,
        _ => 0,
    }
}

/// Status JSON: {"branch":…,"has_changes":…,"files":[{"path":…,"status":…}]}.
#[no_mangle]
pub extern "C" fn grit_repo_status_json(repo: *mut Repository) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    guard_json(|| Ok(serde_json::to_value(ops::status(repo)?)?))
}

#[no_mangle]
pub extern "C" fn grit_repo_current_branch(repo: *mut Repository) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    guard_string(|| ops::current_branch(repo))
}

/// HEAD commit sha, or NULL for an unborn branch (not an error).
#[no_mangle]
pub extern "C" fn grit_repo_head_sha(repo: *mut Repository) -> *mut c_char {
    if repo.is_null() {
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    match ops::head_sha(repo) {
        Some(sha) => out_string(sha),
        None => std::ptr::null_mut(),
    }
}

/// Stage every non-ignored change. 0 on success, -1 on failure.
#[no_mangle]
pub extern "C" fn grit_repo_stage_all(repo: *mut Repository) -> i32 {
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    guard_unit(|| ops::stage_all(repo))
}

/// Commit the index. Returns the new sha or NULL.
#[no_mangle]
pub extern "C" fn grit_repo_commit(
    repo: *mut Repository,
    message: *const c_char,
    author_name: *const c_char,
    author_email: *const c_char,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(message) = (unsafe { take_str(message) }) else {
        set_error("null commit message");
        return std::ptr::null_mut();
    };
    let name = unsafe { take_str(author_name) }.unwrap_or_else(|| "NeuraScribe".to_string());
    let email =
        unsafe { take_str(author_email) }.unwrap_or_else(|| "app@neurascribe.local".to_string());
    guard_string(|| ops::commit(repo, &message, &name, &email))
}

/// Commit only the named paths, the way `git commit -- <paths>` does.
///
/// `paths` is a JSON array of repository-relative paths. Anything else the user
/// had staged stays staged. Returns the new sha or NULL.
#[no_mangle]
pub extern "C" fn grit_repo_commit_only(
    repo: *mut Repository,
    message: *const c_char,
    author_name: *const c_char,
    author_email: *const c_char,
    paths_json: *const c_char,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(message) = (unsafe { take_str(message) }) else {
        set_error("null commit message");
        return std::ptr::null_mut();
    };
    let name = unsafe { take_str(author_name) }.unwrap_or_else(|| "NeuraScribe".to_string());
    let email =
        unsafe { take_str(author_email) }.unwrap_or_else(|| "app@neurascribe.local".to_string());
    let Some(raw) = (unsafe { take_str(paths_json) }) else {
        set_error("null paths");
        return std::ptr::null_mut();
    };
    let paths: Vec<String> = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            set_error(&format!("paths is not a JSON array of strings: {e}"));
            return std::ptr::null_mut();
        }
    };
    guard_string(|| ops::commit_only(repo, &message, &name, &email, &paths))
}

/// Unified diff of one path, HEAD vs work tree.
#[no_mangle]
pub extern "C" fn grit_repo_diff_file(repo: *mut Repository, path: *const c_char) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(path) = (unsafe { take_str(path) }) else {
        set_error("null path");
        return std::ptr::null_mut();
    };
    guard_string(|| ops::diff_file(repo, &path))
}

/// Patches of a commit vs its parent, as JSON [{"path":…,"patch":…}].
#[no_mangle]
pub extern "C" fn grit_repo_diff_commit_json(repo: *mut Repository, sha: *const c_char) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(sha) = (unsafe { take_str(sha) }) else {
        set_error("null sha");
        return std::ptr::null_mut();
    };
    guard_json(|| Ok(serde_json::to_value(ops::diff_commit(repo, &sha)?)?))
}

/// Commits reachable from HEAD as JSON: [{hash, author_time, subject, message}].
///
/// Ordered by AUTHOR time, newest first (see ops::log). `limit` is a hard
/// maximum; a repository shorter than that returns everything it has.
#[no_mangle]
pub extern "C" fn grit_repo_log_json(repo: *mut Repository, limit: usize) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    guard_json(|| Ok(serde_json::to_value(ops::log(repo, limit)?)?))
}
/// Resolve a ref name (or a raw SHA) to a commit SHA, as JSON string.
/// Frees the repo's ability to read packed-refs without the caller parsing it.
#[no_mangle]
pub extern "C" fn grit_repo_resolve_ref(repo: *mut Repository, refname: *const c_char) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(refname) = (unsafe { take_str(refname) }) else {
        set_error("null refname");
        return std::ptr::null_mut();
    };
    guard_json(|| Ok(serde_json::to_value(ops::resolve_ref(repo, &refname)?)?))
}

/// Every tracked path at a commit, as a JSON array of repo-relative paths.
#[no_mangle]
pub extern "C" fn grit_repo_list_files_json(repo: *mut Repository, sha: *const c_char) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(sha) = (unsafe { take_str(sha) }) else {
        set_error("null sha");
        return std::ptr::null_mut();
    };
    guard_json(|| Ok(serde_json::to_value(ops::list_files(repo, &sha)?)?))
}

/// Stage one path (git add -- <path>), including recording a deletion.
/// 0 on success, -1 on failure.
#[no_mangle]
pub extern "C" fn grit_repo_stage_file(repo: *mut Repository, path: *const c_char) -> i32 {
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    let Some(path) = (unsafe { take_str(path) }) else {
        set_error("null path");
        return -1;
    };
    guard_unit(|| ops::stage_file(repo, &path))
}

/// Fast-forward HEAD and the work tree to a commit; returns the new HEAD as a
/// JSON string. Fails when the histories have diverged.
#[no_mangle]
pub extern "C" fn grit_repo_fast_forward_json(repo: *mut Repository, tip: *const c_char) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(tip) = (unsafe { take_str(tip) }) else {
        set_error("null tip");
        return std::ptr::null_mut();
    };
    guard_json(|| Ok(serde_json::to_value(ops::fast_forward_to(repo, &tip)?)?))
}

/// Files differing between two commits, as JSON.
#[no_mangle]
pub extern "C" fn grit_repo_changed_files_json(
    repo: *mut Repository,
    base: *const c_char,
    head: *const c_char,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let (base, head) = match (unsafe { take_str(base) }, unsafe { take_str(head) }) {
        (Some(b), Some(h)) => (b, h),
        _ => {
            set_error("null base or head");
            return std::ptr::null_mut();
        }
    };
    guard_json(|| Ok(serde_json::to_value(ops::changed_files(repo, &base, &head)?)?))
}

/// Restore one path to its HEAD content.
#[no_mangle]
pub extern "C" fn grit_repo_reset_file_to_head(repo: *mut Repository, path: *const c_char) -> i32 {
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    let Some(path) = (unsafe { take_str(path) }) else {
        set_error("null path");
        return -1;
    };
    guard_unit(|| ops::reset_file_to_head(repo, &path))
}

/// Hard reset to `sha`; returns the sha or NULL.
#[no_mangle]
pub extern "C" fn grit_repo_reset_hard_to(repo: *mut Repository, sha: *const c_char) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(sha) = (unsafe { take_str(sha) }) else {
        set_error("null sha");
        return std::ptr::null_mut();
    };
    guard_string(|| ops::reset_hard_to(repo, &sha))
}

/// Discard all work-tree changes; returns the restored HEAD sha or NULL.
#[no_mangle]
pub extern "C" fn grit_repo_force_checkout_head(repo: *mut Repository) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    guard_string(|| ops::force_checkout_head(repo))
}

/// Remotes as JSON [{"name":…,"url":…}].
#[no_mangle]
pub extern "C" fn grit_repo_list_remotes_json(repo: *mut Repository) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    guard_json(|| Ok(serde_json::to_value(ops::list_remotes(repo)?)?))
}

/// Commits HEAD is ahead of `remote_tip`; -1 on failure.
#[no_mangle]
pub extern "C" fn grit_repo_ahead_count(repo: *mut Repository, remote_tip: *const c_char) -> i64 {
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    let Some(tip) = (unsafe { take_str(remote_tip) }) else {
        set_error("null remote tip");
        return -1;
    };
    match ops::ahead_count(repo, &tip) {
        Ok(n) => i64::from(n),
        Err(err) => {
            set_error(&format!("{err:#}"));
            -1
        }
    }
}

/// 1 when `a` is an ancestor of `b`, 0 when not, -1 on failure.
#[no_mangle]
pub extern "C" fn grit_repo_is_ancestor(
    repo: *mut Repository,
    a: *const c_char,
    b: *const c_char,
) -> i32 {
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    let (a, b) = match (unsafe { take_str(a) }, unsafe { take_str(b) }) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            set_error("null commit");
            return -1;
        }
    };
    match ops::is_ancestor(repo, &a, &b) {
        Ok(v) => i32::from(v),
        Err(err) => {
            set_error(&format!("{err:#}"));
            -1
        }
    }
}

/// Best common ancestor; NULL when unrelated or on failure (check
/// `grit_last_error` to tell them apart).
#[no_mangle]
pub extern "C" fn grit_repo_merge_base(
    repo: *mut Repository,
    a: *const c_char,
    b: *const c_char,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let (a, b) = match (unsafe { take_str(a) }, unsafe { take_str(b) }) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            set_error("null commit");
            return std::ptr::null_mut();
        }
    };
    match ops::merge_base(repo, &a, &b) {
        Ok(Some(sha)) => out_string(sha),
        Ok(None) => std::ptr::null_mut(),
        Err(err) => {
            set_error(&format!("{err:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Text of `path` at `sha`; NULL when absent or on failure.
#[no_mangle]
pub extern "C" fn grit_repo_blob_text(
    repo: *mut Repository,
    sha: *const c_char,
    path: *const c_char,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let (sha, path) = match (unsafe { take_str(sha) }, unsafe { take_str(path) }) {
        (Some(s), Some(p)) => (s, p),
        _ => {
            set_error("null sha or path");
            return std::ptr::null_mut();
        }
    };
    match ops::blob_text(repo, &sha, &path) {
        Ok(Some(text)) => out_string(text),
        Ok(None) => std::ptr::null_mut(),
        Err(err) => {
            set_error(&format!("{err:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Absolute work-tree path; NULL for a bare repo or on failure.
#[no_mangle]
pub extern "C" fn grit_repo_work_tree(repo: *mut Repository) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    match &repo.work_tree {
        Some(path) => out_string(path.to_string_lossy().to_string()),
        None => std::ptr::null_mut(),
    }
}

// MARK: - network

/// Optional credentials, as a JSON object `{"username":…,"password":…}`.
/// A null pointer means "no credential".
///
/// A pointer that is present but unreadable is an **error**, never an absent
/// credential. Swallowing the parse failure here meant that a pointer to freed
/// memory arrived as garbage, failed to parse, and became `None` — so
/// a caller that had a perfectly good token in its hand was told, on the far
/// side, that it had no credential at all. The error then read as "you never
/// supplied a token" when the truth was "we lost the one you gave us".
unsafe fn take_credential(json: *const c_char) -> std::result::Result<Option<types::Credential>, String> {
    let Some(raw) = (unsafe { take_str(json) }) else {
        return Ok(None);
    };
    match serde_json::from_str(&raw) {
        Ok(cred) => Ok(Some(cred)),
        Err(err) => Err(format!("credential JSON is unreadable: {err}")),
    }
}


/// Read the per-call HTTP callbacks the embedder passed in.
///
/// A NULL pointer means the embedder wants no progress, which is not an error:
/// progress is a nicety, and an operation that reports none still succeeds.
unsafe fn take_progress(
    p: *const crate::progress::ProgressCallbacks,
) -> crate::progress::ProgressCallbacks {
    if p.is_null() {
        crate::progress::ProgressCallbacks::none()
    } else {
        *p
    }
}

/// A NULL pointer means the embedder has no HTTP bridge, which the client turns
/// into a clear error rather than a silent no-op.
unsafe fn take_callbacks(p: *const crate::http::HttpCallbacks) -> crate::http::HttpCallbacks {
    if p.is_null() {
        crate::http::HttpCallbacks::none()
    } else {
        *p
    }
}

/// Clone `url` into a new repository at `dest`. Returns the checked-out commit
/// sha, or NULL on failure.
#[no_mangle]
pub extern "C" fn grit_clone(
    url: *const c_char,
    dest: *const c_char,
    branch: *const c_char,
    credential: *const c_char,
    http: *const crate::http::HttpCallbacks,
    progress: *const crate::progress::ProgressCallbacks,
) -> *mut c_char {
    let (url, dest) = match (unsafe { take_str(url) }, unsafe { take_str(dest) }) {
        (Some(u), Some(d)) => (u, d),
        _ => {
            set_error("null url or dest");
            return std::ptr::null_mut();
        }
    };
    let branch = unsafe { take_str(branch) };
    let credential = match unsafe { take_credential(credential) } {
        Ok(c) => c,
        Err(msg) => {
            set_error(&msg);
            return std::ptr::null_mut();
        }
    };
    guard_string(|| {
        net::clone(
            &url,
            Path::new(&dest),
            branch.as_deref(),
            credential.as_ref(),
            unsafe { take_callbacks(http) },
            unsafe { take_progress(progress) },
        )
    })
}

/// Fetch into an already-open repository. Returns fetch JSON, or NULL.
#[no_mangle]
pub extern "C" fn grit_repo_fetch_json(
    repo: *mut Repository,
    url: *const c_char,
    refspecs_json: *const c_char,
    credential: *const c_char,
    http: *const crate::http::HttpCallbacks,
    progress: *const crate::progress::ProgressCallbacks,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(url) = (unsafe { take_str(url) }) else {
        set_error("null url");
        return std::ptr::null_mut();
    };
    let refspecs: Vec<String> = unsafe { take_str(refspecs_json) }
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();

    let credential = match unsafe { take_credential(credential) } {
        Ok(c) => c,
        Err(msg) => {
            set_error(&msg);
            return std::ptr::null_mut();
        }
    };
    guard_json(move || {
        let result = net::fetch(
            &repo.git_dir,
            &url,
            &refspecs,
            credential.as_ref(),
            unsafe { take_callbacks(http) },
            unsafe { take_progress(progress) },
        )?;
        serde_json::to_value(result).map_err(anyhow::Error::from)
    })
}

/// Push `branch` to the same-named branch on `url`. Returns push JSON, or NULL.
#[no_mangle]
pub extern "C" fn grit_repo_push_json(
    repo: *mut Repository,
    url: *const c_char,
    branch: *const c_char,
    force: i32,
    credential: *const c_char,
    http: *const crate::http::HttpCallbacks,
    progress: *const crate::progress::ProgressCallbacks,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let (url, branch) = match (unsafe { take_str(url) }, unsafe { take_str(branch) }) {
        (Some(u), Some(b)) => (u, b),
        _ => {
            set_error("null url or branch");
            return std::ptr::null_mut();
        }
    };
    let credential = match unsafe { take_credential(credential) } {
        Ok(c) => c,
        Err(msg) => {
            set_error(&msg);
            return std::ptr::null_mut();
        }
    };
    let force = force != 0;
    guard_json(move || {
        let result = net::push(
            &repo.git_dir,
            &url,
            &branch,
            force,
            credential.as_ref(),
            unsafe { take_callbacks(http) },
            unsafe { take_progress(progress) },
        )?;
        serde_json::to_value(result).map_err(anyhow::Error::from)
    })
}

// ---- submodules -----------------------------------------------------------

/// Every `submodule.<name>` section in `.gitmodules`, as JSON.
#[no_mangle]
pub unsafe extern "C" fn grit_repo_submodules_json(
    repo: *mut Repository,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    guard_json(move || {
        let list = submodules::submodules(&repo)?;
        serde_json::to_value(list).map_err(anyhow::Error::from)
    })
}

/// Record a submodule: write its `.gitmodules` section and stage its gitlink.
///
/// The clone must already exist; this only records it.
#[no_mangle]
pub unsafe extern "C" fn grit_repo_record_submodule(
    repo: *mut Repository,
    name: *const c_char,
    path: *const c_char,
    url: *const c_char,
    sha: *const c_char,
) -> i32 {
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    let (Some(name), Some(path), Some(url), Some(sha)) = (
        unsafe { take_str(name) },
        unsafe { take_str(path) },
        unsafe { take_str(url) },
        unsafe { take_str(sha) },
    ) else {
        set_error("null name, path, url or sha");
        return -1;
    };
    guard_unit(move || submodules::record_submodule(&repo, &name, &path, &url, &sha))
}

/// Drop a submodule's registration, gitlink, work tree and own repository.
#[no_mangle]
pub unsafe extern "C" fn grit_repo_remove_submodule(
    repo: *mut Repository,
    path: *const c_char,
) -> i32 {
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    let Some(path) = (unsafe { take_str(path) }) else {
        set_error("null path");
        return -1;
    };
    guard_unit(move || submodules::remove_submodule(&repo, &path))
}

/// Check the submodule out at the commit the superproject records.
#[no_mangle]
pub unsafe extern "C" fn grit_repo_checkout_submodule_to(
    repo: *mut Repository,
    path: *const c_char,
    sha: *const c_char,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let (Some(path), Some(sha)) = (unsafe { take_str(path) }, unsafe { take_str(sha) }) else {
        set_error("null path or sha");
        return std::ptr::null_mut();
    };
    guard_string(move || submodules::checkout_submodule_to(&repo, &path, &sha))
}

/// Paths the index tracks below a directory, as a JSON array of strings.
///
/// Answers "is anything under this directory still tracked" without a
/// directory listing, so files present on disk but never added do not count.
#[no_mangle]
pub unsafe extern "C" fn grit_repo_indexed_paths_under_json(
    repo: *mut Repository,
    directory: *const c_char,
) -> *mut c_char {
    if repo.is_null() {
        set_error("null repo");
        return std::ptr::null_mut();
    }
    let repo = unsafe { &*repo };
    let Some(directory) = (unsafe { take_str(directory) }) else {
        set_error("null directory");
        return std::ptr::null_mut();
    };
    guard_json(move || Ok(serde_json::to_value(index_ops::indexed_paths_under(&repo, &directory)?)?))
}

/// Stop tracking everything under a directory, leaving the files on disk.
///
/// This is `git rm -r --cached`. Returns the number of index entries
/// removed, or -1 on failure.
#[no_mangle]
pub unsafe extern "C" fn grit_repo_untrack_directory(
    repo: *mut Repository,
    directory: *const c_char,
) -> i32 {
    if repo.is_null() {
        set_error("null repo");
        return -1;
    }
    let repo = unsafe { &*repo };
    let Some(directory) = (unsafe { take_str(directory) }) else {
        set_error("null directory");
        return -1;
    };
    match index_ops::untrack_directory(&repo, &directory) {
        Ok(n) => n as i32,
        Err(e) => {
            set_error(&e.to_string());
            -1
        }
    }
}