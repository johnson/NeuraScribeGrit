//! Network operations: clone, fetch and push over smart HTTP.
//!
//! All three run on grit's own protocol implementations -- `http_fetch` and
//! `push_http` do the full smart-HTTP dance, and clone is init + fetch +
//! checkout. The only thing this module supplies is grit's `HttpClient`, the
//! credential, and turning the results into the shapes the Swift side already
//! knows from the gix backend.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use grit_lib::transfer::{FetchOptions, PushOptions, PushRefSpec, TagMode};

use crate::http;
use crate::ops;
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

/// Fetch from `url` into the repository whose git dir is `git_dir`.
pub fn fetch(
    git_dir: &Path,
    url: &str,
    refspecs: &[String],
    cred: Option<&Credential>,
) -> Result<FetchResult> {
    let client = http::client_for(cred)?;
    let base = http_base_url(url);
    let outcome = grit_lib::transport::http::http_fetch(
        client.as_ref(),
        git_dir,
        &base,
        &fetch_options(refspecs.to_vec()),
        &mut grit_lib::fetch::NoProgress,
    )
    .with_context(|| format!("fetch from {base} failed"))?;

    let mut updated_refs = BTreeMap::new();
    for update in &outcome.updates {
        if let Some(oid) = &update.new_oid {
            updated_refs.insert(update.remote_ref.clone(), oid.to_string());
        }
    }
    Ok(FetchResult { updated_refs, default_branch: outcome.default_branch.clone() })
}

/// Clone `url` into a fresh repository at `dest`, checking out `branch` or the
/// server's default branch. Returns the checked-out commit sha.
pub fn clone(
    url: &str,
    dest: &Path,
    branch: Option<&str>,
    cred: Option<&Credential>,
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

    let client = http::client_for(cred)?;
    // grit's fetch has no "fetch everything" mode -- an empty refspec list
    // writes nothing -- so the remote-tracking namespace is populated first
    // and the branch is created from it afterwards. `git clone` orders it
    // the same way.
    let outcome = grit_lib::transport::http::http_fetch(
        client.as_ref(),
        &repo.git_dir,
        &base,
        &fetch_options(vec![format!("+refs/heads/*:{TRACKING_PREFIX}*")]),
        &mut grit_lib::fetch::NoProgress,
    )
    .with_context(|| format!("clone from {base} failed"))?;

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

    let client = http::client_for(cred)?;
    let base = http_base_url(url);
    let outcome = grit_lib::push::push_http(
        client.as_ref(),
        git_dir,
        &base,
        &refs,
        &opts,
        &mut grit_lib::fetch::NoProgress,
    )
    .with_context(|| format!("push to {base} failed"))?;

    let mut pushed = Vec::new();
    for result in &outcome.results {
        let (status, reason) = match &result.status {
            grit_lib::push_report::PushRefStatus::Ok => ("ok", None),
            grit_lib::push_report::PushRefStatus::UpToDate => ("up_to_date", None),
            other => ("rejected", result.message.clone().or_else(|| Some(format!("{other:?}")))),
        };
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
