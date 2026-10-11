//! Network operations: clone, fetch and push over smart HTTP, the native
//! git daemon protocol (fetch only), and local directories.
//!
//! The HTTP operations run on grit's own protocol implementations -- `http_fetch`
//! and `push_http` do the full smart-HTTP dance, and clone is init + fetch +
//! checkout. git:// fetches go through grit's `GitDaemonTransport` (raw TCP,
//! no HTTP bridge involved). The only things this module supplies are grit's
//! `HttpClient`, the credential, and turning the results into the shapes the
//! Swift side already knows from the gix backend.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use grit_lib::transfer::{FetchOptions, FetchOutcome, PushOptions, PushRefSpec, TagMode};
use grit_lib::transport::Transport;

use crate::http;
use crate::ops;
use crate::progress::{ByteCounters, ProgressBridge, ProgressCallbacks, ProgressReporter};
use crate::types::Credential;

/// A finished fetch, in the shape the Swift side already decodes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FetchResult {
    /// Refs the server had, `name -> oid`, for the branches that were asked for.
    pub updated_refs: BTreeMap<String, String>,
    /// The remote's default branch name, when the server advertised one.
    pub default_branch: Option<String>,
}

/// A finished push, mirroring grit's own result types closely enough that the
/// Swift side can render per-ref outcomes the way it renders gix's.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PushResult {
    pub refs: Vec<PushedRef>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PushedRef {
    /// Fully qualified remote ref, e.g. `refs/heads/main`.
    pub name: String,
    /// `ok`, `up_to_date`, or `rejected`.
    pub status: String,
    /// Server-supplied explanation when the status is not a success.
    pub reason: Option<String>,
}

/// Normalise a remote URL into the base smart-HTTP builds on.
///
/// The app stores whatever the user typed, and git accepts several spellings
/// for the same remote. Anything unrecognised is passed through unchanged and
/// left for the server to reject -- guessing here would be worse than a clear
/// server error.
pub fn http_base_url(url: &str) -> String {
    let mut base = url.trim().to_string();
    // scp-style `user@host:path` only when there is no scheme and no slash
    // before the colon; otherwise the colon is a port in a real URL.
    if !base.contains("://") {
        if let Some(colon) = base.find(':') {
            if !base[..colon].contains('/') {
                let user_host = &base[..colon];
                let path = base[colon + 1..].trim_start_matches('/');
                base = format!("http://{user_host}/{path}");
            }
        }
    }
    if base.ends_with('/') {
        base.pop();
    }
    if let Some(stripped) = base.strip_suffix(".git") {
        base = stripped.to_string();
    }
    base
}

fn fetch_options(refspecs: Vec<String>) -> FetchOptions {
    FetchOptions {
        refspecs,
        negative_refspecs: Vec::new(),
        // Follow tags, like the backend this replaces does.
        tags: TagMode::All,
        prune: false,
        dry_run: false,
        depth: None,
        deepen_since: None,
        deepen_not: Vec::new(),
        unshallow: false,
    }
}

impl FetchResult {
    fn from_outcome(outcome: &FetchOutcome) -> Self {
        let mut updated_refs = BTreeMap::new();
        for update in &outcome.updates {
            if let Some(oid) = &update.new_oid {
                // Key by the ref that was actually written, not the remote one.
                // The two differ whenever a refspec has a destination, and the
                // caller goes on to read the local ref.
                let key = update.local_ref.as_ref().unwrap_or(&update.remote_ref);
                updated_refs.insert(key.clone(), oid.to_string());
            }
        }
        FetchResult { updated_refs, default_branch: outcome.default_branch.clone() }
    }
}

/// The git dir a local remote lives in, when `url` names one on disk.
///
/// Handles a plain path (`/srv/repo`, `../other`) and a `file://` URL. A
/// `file://` URL with a host other than empty or localhost is rejected, because
/// that is a remote file share and pretending it is local would read the wrong
/// machine's disk.
fn local_remote_path(url: &str) -> Option<std::path::PathBuf> {
    let trimmed = url.trim();
    if let Some(rest) = trimmed.strip_prefix("file://") {
        // The authority runs to the first slash, so `file:///p` has an empty
        // host and `file://localhost/p` has `localhost`; both are local.
        // `file://otherhost/p` is a share on another machine and must not be
        // read off this one's disk.
        let mut parts = rest.splitn(2, '/');
        let host = parts.next().unwrap_or_default();
        let path = parts.next()?;
        if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
            return None;
        }
        // `path` lost the separator the split consumed; the result is always
        // absolute.
        return Some(std::path::PathBuf::from("/").join(path));
    }
    if trimmed.contains("://") {
        return None;
    }
    if trimmed.starts_with("//") {
        // scp-style `user@host:path`; never a local path.
        return None;
    }
    if trimmed.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(trimmed))
}
/// Announce the refs a fetch moved, then the byte total.
///
/// `RefUpdate` carries the source ref, the local ref it was written to, and both
/// OIDs, so this is the `update_tips` hook with real values rather than a
/// reconstruction. `received_bytes` is measured at the HTTP boundary and is 0
/// for a local remote, which has no such boundary.
fn report_fetch(reporter: &ProgressReporter, outcome: &FetchOutcome, received_bytes: u64) {
    let mut moved = 0u32;
    for update in &outcome.updates {
        let Some(new_oid) = &update.new_oid else { continue };
        let refname = update.local_ref.as_ref().unwrap_or(&update.remote_ref);
        // Both OIDs have to live past the call, so bind them first; each
        // `to_hex` is a fresh String that would drop at the end of the
        // argument list.
        let old_hex = update.old_oid.map(|o| o.to_hex());
        let new_hex = new_oid.to_hex();
        reporter.update_tip(refname, old_hex.as_deref(), Some(&new_hex));
        moved += 1;
    }
    if moved > 0 || received_bytes > 0 {
        reporter.transfer(moved, received_bytes);
    }
}

/// Fetch from `url` into the repository whose git dir is `git_dir`.
pub fn fetch(
    git_dir: &Path,
    url: &str,
    refspecs: &[String],
    cred: Option<&Credential>,
    callbacks: http::HttpCallbacks,
    progress: ProgressCallbacks,
) -> Result<FetchResult> {
    // A local remote must not go through the HTTP bridge. URLSession has no
    // file:// transport, so the request fails before it starts; and even if
    // it did, it would be the wrong path -- git serves a local remote straight
    // from the object database. grit already has that transport, so use it.
    let reporter = ProgressReporter::new(progress);

    if let Some(remote_git_dir) = local_remote_path(url) {
        let outcome = grit_lib::transfer::fetch_local(
            git_dir,
            &remote_git_dir,
            &fetch_options(refspecs.to_vec()),
        )
        .with_context(|| format!("fetch from {} failed", remote_git_dir.display()))?;
        // No HTTP boundary, so no byte total to report; the ref updates
        // below are still real.
        report_fetch(&reporter, &outcome, 0);
        return Ok(FetchResult::from_outcome(&outcome));
    }

    // A git:// URL speaks the native daemon protocol over a raw TCP socket.
    // URLSession has no handler for that scheme, but the daemon transport is
    // self-contained (no HTTP at all), so it works on iOS exactly as on the
    // desktop -- and it is the natural choice for LAN sync with a machine on
    // the same network (no TLS, no credentials, one port).
    if url.trim_start().starts_with("git://") {
        let transport = grit_lib::transport::GitDaemonTransport::new();
        let mut conn = transport
            .connect(
                url.trim(),
                grit_lib::transport::Service::UploadPack,
                &grit_lib::transport::ConnectOptions::default(),
            )
            .with_context(|| format!("connect to {url} failed"))?;
        let mut bridge = ProgressBridge::new(progress);
        let outcome = grit_lib::fetch::fetch_remote(
            git_dir,
            conn.as_mut(),
            &fetch_options(refspecs.to_vec()),
            &mut bridge,
        )
        .with_context(|| format!("fetch from {url} failed"))?;
        // Raw TCP has no HTTP boundary to measure bytes at, same as a local
        // remote; the ref updates are still real.
        report_fetch(&reporter, &outcome, 0);
        return Ok(FetchResult::from_outcome(&outcome));
    }

    let counters = Arc::new(ByteCounters::default());
    let client = http::client_for(cred, callbacks, counters.clone())?;
    let base = http_base_url(url);
    let mut bridge = ProgressBridge::new(progress);
    let outcome = grit_lib::transport::http::http_fetch(
        client.as_ref(),
        git_dir,
        &base,
        &fetch_options(refspecs.to_vec()),
        &mut bridge,
    )
    .with_context(|| format!("fetch from {base} failed"))?;

    report_fetch(&reporter, &outcome, counters.received());
    Ok(FetchResult::from_outcome(&outcome))
}

/// Clone `url` into a fresh repository at `dest`, checking out `branch` or the
/// server's default branch. Returns the checked-out commit sha.
pub fn clone(
    url: &str,
    dest: &Path,
    branch: Option<&str>,
    cred: Option<&Credential>,
    callbacks: http::HttpCallbacks,
    progress: ProgressCallbacks,
) -> Result<String> {
    if dest.exists() {
        bail!("clone target already exists: {}", dest.display());
    }
    // A concrete name is needed up front: HEAD is written as a symref, and on
    // an unborn branch the clone would leave an empty work tree.
    let repo = grit_lib::repo::init_repository(dest, false, "main", None, "files")
        .with_context(|| format!("could not create a repository at {}", dest.display()))?;

    let base = http_base_url(url);
    write_remote(&repo.git_dir, "origin", &base)?;

    let reporter = ProgressReporter::new(progress);
    let counters = Arc::new(ByteCounters::default());
    let client = http::client_for(cred, callbacks, counters.clone())?;
    let mut bridge = ProgressBridge::new(progress);

    // grit's fetch has no "fetch everything" mode -- an empty refspec list
    // writes nothing -- so the remote-tracking namespace is populated first
    // and the branch is created from it afterwards. `git clone` orders it
    // the same way.
    let outcome = grit_lib::transport::http::http_fetch(
        client.as_ref(),
        &repo.git_dir,
        &base,
        &fetch_options(vec![format!("+refs/heads/*:{TRACKING_PREFIX}*")]),
        &mut bridge,
    )
    .with_context(|| format!("clone from {base} failed"))?;
    report_fetch(&reporter, &outcome, counters.received());

    let name = match branch {
        Some(name) => name.to_string(),
        None => outcome.default_branch.clone().unwrap_or_else(|| "main".to_string()),
    };
    let head = grit_lib::refs::resolve_ref(&repo.git_dir, &format!("{TRACKING_PREFIX}{name}"))
        .with_context(|| format!("the server has no branch named {name}"))?;
    grit_lib::refs::write_ref(&repo.git_dir, &format!("refs/heads/{name}"), &head)?;
    ops::checkout_branch(&repo, &name, &head.to_string())?;
    Ok(head.to_string())
}

/// Push local `branch` to `refs/heads/branch` on `url`.
///
/// `force` maps to git's `+` refspec. Everything else -- the advertisement,
/// the ref decision, the pack, and reading back the server's verdict -- is
/// grit's `push_http`.
pub fn push(
    git_dir: &Path,
    url: &str,
    branch: &str,
    force: bool,
    cred: Option<&Credential>,
    callbacks: http::HttpCallbacks,
    progress: ProgressCallbacks,
) -> Result<PushResult> {
    let local_ref = format!("refs/heads/{branch}");
    let src: grit_lib::objects::ObjectId = grit_lib::refs::resolve_ref(git_dir, &local_ref)
        .with_context(|| format!("no local branch named {branch}"))?;

    let refs = [PushRefSpec {
        src: Some(src),
        dst: local_ref.clone(),
        force,
        delete: false,
        expected_old: None,
        expect_absent: false,
    }];
    let opts = PushOptions {
        atomic: false,
        dry_run: false,
        push_options: Vec::new(),
    };

    let reporter = ProgressReporter::new(progress);
    // The one update about to be sent. The old value is whatever the server
    // holds and is not known until the advertisement comes back, so it is
    // omitted rather than guessed.
    reporter.push_negotiation(&local_ref, &src.to_hex());
    reporter.pack(0);

    let counters = Arc::new(ByteCounters::default());
    let client = http::client_for(cred, callbacks, counters.clone())?;
    let mut bridge = ProgressBridge::new(progress);

    let base = http_base_url(url);
    let outcome = grit_lib::push::push_http(
        client.as_ref(),
        git_dir,
        &base,
        &refs,
        &opts,
        &mut bridge,
    )
    .with_context(|| format!("push to {base} failed"))?;
    reporter.pack(1);
    reporter.push_transfer(counters.sent());

    let mut pushed = Vec::new();
    for result in &outcome.results {
        let (status, reason) = match &result.status {
            grit_lib::push_report::PushRefStatus::Ok => ("ok", None),
            grit_lib::push_report::PushRefStatus::UpToDate => ("up_to_date", None),
            other => ("rejected", result.message.clone().or_else(|| Some(format!("{other:?}")))),
        };
        // The verdict the server gave, announced as it is decided rather than
        // replayed at the end, so a slow push can show which ref is still
        // outstanding.
        reporter.push_update_ref(&result.remote_ref, reason.as_deref());
        pushed.push(PushedRef {
            name: result.remote_ref.clone(),
            status: status.to_string(),
            reason,
        });
    }
    Ok(PushResult { refs: pushed })
}

/// Where a clone parks remote-tracking refs before branching off one of them.
const TRACKING_PREFIX: &str = "refs/remotes/origin/";

/// Record a remote in the repository config.
pub fn write_remote(git_dir: &Path, name: &str, url: &str) -> Result<()> {
    let path = git_dir.join("config");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "[remote \"{name}\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/{name}/*\n"
    ));
    std::fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}
