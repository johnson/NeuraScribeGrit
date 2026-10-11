//! A git:// remote must be fetched over the native daemon transport, not the
//! HTTP bridge.
//!
//! The Swift side answers HTTP with URLSession, which has no handler for the
//! git scheme -- routing a git:// URL there fails before a byte moves. grit
//! ships GitDaemonTransport (raw TCP), and these tests pin that the fetch
//! dispatch takes it, against a real git-daemon process (interop with the
//! real server, not grit's own).

use std::path::Path;
use std::process::{Child, Command, Stdio};

use neurascribe_grit_ffi::{http::HttpCallbacks, net, ops, progress::ProgressCallbacks};

fn no_progress() -> ProgressCallbacks {
    ProgressCallbacks::none()
}

/// A git:// fetch never reaches the HTTP bridge, so this is what the call
/// site passes to prove it.
fn no_http() -> HttpCallbacks {
    HttpCallbacks::none()
}

const REFS: &str = "refs/heads/*:refs/remotes/origin/*";

/// A port nobody else is using right now. A fixed port collides with the
/// zombie a crashed earlier run leaves behind (its tempdir is gone, so every
/// connection to it is RST -- which then gets misread as a transport bug).
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    l.local_addr().expect("addr").port()
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repository acting as the remote, with one commit on main, seeded
/// through a scratch work tree.
fn remote(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("srv").join("remote.git");
    std::fs::create_dir_all(&path).expect("mkdir");
    git(&path, &["init", "--bare", "--initial-branch=main"]);
    // git-daemon only exports repos marked with this file when --export-all
    // is not given; keep the daemon invocation explicit either way.
    std::fs::write(path.join("git-daemon-export-ok"), "").expect("export marker");

    let seed = dir.join("seed");
    std::fs::create_dir_all(&seed).expect("mkdir seed");
    std::fs::write(seed.join("shared.md"), "base
").expect("write");
    git(&seed, &["init", "--initial-branch=main"]);
    git(&seed, &["add", "."]);
    git(&seed, &["-c", "user.email=t@example.com", "-c", "user.name=T", "commit", "-m", "seed"]);
    git(&seed, &["push", path.to_str().expect("utf8"), "main"]);
    path
}

struct Daemon(Child);

impl Daemon {
    fn start(base: &Path, port: u16) -> Daemon {
        let child = Command::new("git")
            .args([
                "daemon",
                "--reuseaddr",
                &format!("--base-path={}", base.display()),
                "--listen=127.0.0.1",
                &format!("--port={port}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("git daemon spawns");
        // The listen socket is up before spawn returns in practice; give the
        // scheduler a beat anyway so the fetch does not race the bind.
        std::thread::sleep(std::time::Duration::from_millis(300));
        Daemon(child)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
fn git_scheme_fetches_without_the_http_bridge() {
    let dir = tempfile::tempdir().expect("temp");
    let _remote = remote(dir.path());
    let port = free_port();
    let _daemon = Daemon::start(&dir.path().join("srv"), port);
    let local = empty_local(dir.path());

    let url = format!("git://127.0.0.1:{port}/remote.git");
    let result = net::fetch(
        local.as_path(),
        &url,
        &[REFS.to_string()],
        None,
        no_http(),
        no_progress(),
    )
    .expect("a git:// remote must not need the HTTP bridge");

    let remote_main = git(&dir.path().join("srv").join("remote.git"), &["rev-parse", "main"]);
    assert_eq!(
        result.updated_refs.get("refs/remotes/origin/main").map(String::as_str),
        Some(remote_main.as_str()),
        "the fetched ref must land under the refspec destination"
    );

    // And the objects must really be there: the remote tip resolves in the
    // local object database (HEAD is still unborn here -- a fetch into an
    // empty repo moves refs, not the checked-out branch -- so ask the object
    // store directly instead of walking a log).
    let kind = git(&dir.path().join("local"), &["cat-file", "-t", &remote_main]);
    assert_eq!(kind, "commit", "the fetched pack must be ingested, not just refs moved");
    let subject = git(&dir.path().join("local"), &["log", "--format=%s", "-1", &remote_main]);
    assert_eq!(subject, "seed");
}
