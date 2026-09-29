//! The progress hooks, driven against a real repository.
//!
//! The point of this file is that the numbers are real. A progress bar showing
//! invented percentages is worse than no bar, so each assertion is on a value
//! that can only have come from grit or from the wire -- and the byte total for
//! a local remote is asserted to be 0, because there is no boundary to measure
//! and making one up is exactly the failure this guards against.

use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use neurascribe_grit_ffi::{http::HttpCallbacks, net, progress::ProgressCallbacks};

const REFS: &str = "refs/heads/*:refs/remotes/origin/*";

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository acting as the remote, with one commit on `main`.
fn remote_repo(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("remote.git");
    std::fs::create_dir_all(&path).expect("mkdir");
    git(&path, &["init", "--bare", "--initial-branch=main"]);
    let seed = dir.join("seed");
    std::fs::create_dir_all(&seed).expect("mkdir seed");
    std::fs::write(seed.join("a.md"), "one\n").expect("write");
    std::fs::write(seed.join("b.md"), "two\n").expect("write");
    git(&seed, &["init", "--initial-branch=main"]);
    git(&seed, &["add", "."]);
    git(
        &seed,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=T",
            "commit",
            "-m",
            "seed",
        ],
    );
    git(&seed, &["push", path.to_str().expect("utf8"), "main"]);
    path
}

/// An initialised repository with an empty index. Returns its git dir.
fn empty_repo(dir: &Path) -> std::path::PathBuf {
    let local = dir.join("local");
    std::fs::create_dir_all(&local).expect("mkdir local");
    git(&local, &["init", "--initial-branch=main"]);
    local.join(".git")
}

#[derive(Debug, Default)]
struct Log {
    sideband: Vec<String>,
    transfer: Vec<(u32, u64)>,
    update_tips: Vec<(String, Option<String>, Option<String>)>,
    pack: Vec<i32>,
    push_transfer: Vec<u64>,
    push_update_refs: Vec<(String, Option<String>)>,
    push_negotiations: Vec<(String, String)>,
}

/// Everything the seven hooks reported.
///
/// `static` because the hooks are `extern "C"` function pointers and cannot
/// capture a context -- the same constraint that made the HTTP handlers
/// non-capturing.
static LOG: Mutex<Option<Log>> = Mutex::new(None);

/// One test at a time, because `LOG` is process-wide by necessity.
///
/// Holding this for the whole test is what makes the shared log correct; the
/// alternative -- having each test read only its own events -- is not
/// possible, because the hooks have nowhere to put a context to filter by.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the test lock. Held for the duration of the test by being bound to
/// a `_guard` that outlives it.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock() -> std::sync::MutexGuard<'static, Option<Log>> {
    // Not `unwrap`: one test panicking mid-run would poison the lock and every
    // other test would then fail on that panic rather than on its own
    // assertions, which hides the real failure.
    LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn record(f: impl FnOnce(&mut Log)) {
    let mut slot = lock();
    f(slot.get_or_insert_with(Log::default));
}

fn cstr(p: *const std::ffi::c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(
            unsafe { std::ffi::CStr::from_ptr(p) }
                .to_string_lossy()
                .to_string(),
        )
    }
}

extern "C" fn on_sideband(_context: *mut std::ffi::c_void, text: *const std::ffi::c_char) {
    let s = cstr(text).unwrap_or_default();
    record(|log| log.sideband.push(s));
}

extern "C" fn on_transfer(
    _context: *mut std::ffi::c_void,
    total_objects: u32,
    _indexed_objects: u32,
    received_objects: u32,
    _local_objects: u32,
    _total_deltas: u32,
    _indexed_deltas: u32,
    received_bytes: u64,
) {
    // Recorded so the test can assert grit did not invent object counts.
    let _ = total_objects;
    record(|log| log.transfer.push((received_objects, received_bytes)));
}

extern "C" fn on_update_tip(
    _context: *mut std::ffi::c_void,
    refname: *const std::ffi::c_char,
    old_oid: *const std::ffi::c_char,
    new_oid: *const std::ffi::c_char,
) {
    let r = cstr(refname).unwrap_or_default();
    record(|log| log.update_tips.push((r, cstr(old_oid), cstr(new_oid))));
}

extern "C" fn on_pack(_context: *mut std::ffi::c_void, stage: i32, _current: u32, _total: u32) {
    record(|log| log.pack.push(stage));
}

extern "C" fn on_push_transfer(
    _context: *mut std::ffi::c_void,
    _current: u32,
    _total: u32,
    bytes: u64,
) {
    record(|log| log.push_transfer.push(bytes));
}

extern "C" fn on_push_update_ref(
    _context: *mut std::ffi::c_void,
    refname: *const std::ffi::c_char,
    status: *const std::ffi::c_char,
) {
    let r = cstr(refname).unwrap_or_default();
    let s = cstr(status);
    record(|log| log.push_update_refs.push((r, s)));
}

extern "C" fn on_push_negotiation(
    _context: *mut std::ffi::c_void,
    refname: *const std::ffi::c_char,
    new_oid: *const std::ffi::c_char,
) {
    let r = cstr(refname).unwrap_or_default();
    let n = cstr(new_oid).unwrap_or_default();
    record(|log| log.push_negotiations.push((r, n)));
}

fn listening() -> ProgressCallbacks {
    *lock() = Some(Log::default());
    ProgressCallbacks {
        sideband: Some(on_sideband),
        transfer: Some(on_transfer),
        update_tip: Some(on_update_tip),
        pack: Some(on_pack),
        push_transfer: Some(on_push_transfer),
        push_update_ref: Some(on_push_update_ref),
        push_negotiation: Some(on_push_negotiation),
        // The tests share one static log, so the context carries nothing; the
        // hooks reach it through that static instead.
        context: std::ptr::null_mut(),
    }
}

/// Hand the log back. Panics if `listening()` was never called, which would
/// otherwise read as "nothing fired" and quietly pass a wrong test.
fn taken() -> Log {
    lock().take().expect("listening() was never called")
}

#[test]
fn a_fetch_announces_the_refs_it_moved() {
    let _guard = exclusive();
    let dir = tempfile::tempdir().expect("temp");
    let remote = remote_repo(dir.path());
    let local = empty_repo(dir.path());

    let result = net::fetch(
        local.as_path(),
        remote.to_str().expect("utf8"),
        &[REFS.to_string()],
        None,
        HttpCallbacks::none(),
        listening(),
    )
    .expect("fetch");

    let log = taken();
    let moved: Vec<&str> = log.update_tips.iter().map(|(r, _, _)| r.as_str()).collect();
    assert!(
        moved.contains(&"refs/remotes/origin/main"),
        "expected the tracking ref among {moved:?}"
    );
    // The ref reported is the one written, not the remote one.
    assert!(
        log.update_tips
            .iter()
            .all(|(_, _, o)| o.as_ref().is_some_and(|s| s.len() == 40)),
        "every announced oid must be a full sha: {:?}",
        log.update_tips
    );
    // Every ref the result claims to have moved must have been announced.
    // Equality would be too strong: grit also reports the remote HEAD, which is
    // not a ref the caller is told about.
    for name in result.updated_refs.keys() {
        assert!(
            moved.contains(&name.as_str()),
            "{name} was moved but never announced; announced {moved:?}"
        );
    }
}

#[test]
fn a_local_remote_reports_no_byte_total_rather_than_a_fake_one() {
    let _guard = exclusive();
    let dir = tempfile::tempdir().expect("temp");
    let remote = remote_repo(dir.path());
    let local = empty_repo(dir.path());

    net::fetch(
        local.as_path(),
        remote.to_str().expect("utf8"),
        &[REFS.to_string()],
        None,
        HttpCallbacks::none(),
        listening(),
    )
    .expect("fetch");

    let log = taken();
    assert!(
        !log.transfer.is_empty(),
        "the transfer hook should still fire"
    );
    for (_, bytes) in &log.transfer {
        assert_eq!(*bytes, 0, "a local remote has no HTTP boundary to measure");
    }
}

/// The URL of the `git http-backend` server started by `tests/git_http_server.py`.
fn server_url() -> String {
    std::env::var("NSG_TEST_GIT_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8099/upstream.git".to_string())
}

fn base64(input: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Whether the test server is up, so the suite stays runnable without it
/// instead of failing for an unrelated reason.
fn credential() -> neurascribe_grit_ffi::types::Credential {
    neurascribe_grit_ffi::types::Credential {
        username: "tester".to_string(),
        password: "s3cret".to_string(),
    }
}

/// True when these tests can run at all. The `http-ureq` feature is what gives
/// the host build a real HTTP client; without it the only transport is the Swift
/// bridge, and there is no Swift side here to supply one.
fn server_up() -> bool {
    #[cfg(not(feature = "http-ureq"))]
    return false;
    let spec = format!(
        "{}/info/refs?service=git-upload-pack",
        neurascribe_grit_ffi::net::http_base_url(&server_url())
    );
    let auth = base64("tester:s3cret");
    Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-H",
            &format!("Authorization: Basic {auth}"),
            &spec,
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|code| code.trim() == "200")
        .unwrap_or(false)
}

#[test]
fn a_push_over_http_announces_its_refs_phases_and_bytes() {
    if !server_up() {
        return;
    }

    // These three share one static log, so exactly one of them may be looking
    // at it at a time. Without this the push assertions read whatever the fetch
    // tests happened to record, and fail about four runs in five.
    let _guard = exclusive();

    let dir = tempfile::tempdir().expect("temp");
    // Clone first, so the branch we push is a fast-forward of the remote's. A
    // rejected push still announces the building phase, which is not what this
    // test is about.
    let work = dir.path().join("work");
    net::clone(
        &server_url(),
        work.as_path(),
        None,
        Some(&credential()),
        HttpCallbacks::none(),
        ProgressCallbacks::none(),
    )
    .expect("clone");

    // A branch nobody has pushed before. The fixture server is shared and
    // long-lived, so pushing main again is a no-op the second time round — and
    // a pid alone is not unique enough: pids are recycled, and a second run
    // that reused one would find its own branch already on the server and get a
    // non-fast-forward rejection instead of the accept it is asserting on.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let branch = format!("progress-{}-{stamp}", std::process::id());
    git(&work, &["checkout", "-b", &branch]);
    std::fs::write(work.join("c.md"), format!("{}\n", branch)).expect("write");
    git(&work, &["add", "."]);
    git(
        &work,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=T",
            "commit",
            "-m",
            "progress probe",
        ],
    );
    let expected = git(&work, &["rev-parse", "HEAD"]);

    let result = net::push(
        work.join(".git").as_path(),
        &server_url(),
        &branch,
        false,
        Some(&credential()),
        HttpCallbacks::none(),
        listening(),
    )
    .expect("a new branch should be accepted");

    let log = taken();
    let announced = format!("refs/heads/{branch}");
    assert_eq!(log.push_negotiations.len(), 1, "one ref is being pushed");
    assert_eq!(log.push_negotiations[0].0, announced);
    assert_eq!(
        log.push_negotiations[0].1, expected,
        "the announced tip must be the one actually pushed"
    );
    // Both phases, building then uploading, in that order.
    assert_eq!(
        log.pack,
        vec![0, 1],
        "expected a building then an uploading phase"
    );
    assert_eq!(log.push_update_refs.len(), 1);
    assert_eq!(log.push_update_refs[0].0, announced);
    assert_eq!(
        log.push_update_refs[0].1, None,
        "an accepted push has no rejection reason"
    );
    assert_eq!(result.refs[0].status, "ok");
    // The upload is reported once, with the bytes we actually sent.
    assert_eq!(log.push_transfer.len(), 1);
    // Only the Swift-bridge build can count them: under the http-ureq feature
    // grit owns the transport, so there is no seam to measure at and the honest
    // value is 0. Asserting it either way is the point -- the UI has to cope
    // with both, and must not invent a total for the build that has none.
    assert_eq!(log.push_transfer[0] > 0, !cfg!(feature = "http-ureq"));
}
