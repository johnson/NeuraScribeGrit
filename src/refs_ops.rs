//! Branch operations assembled from grit-lib ref primitives.
//!
//! grit-lib ships the ref store (`refs::list_refs` / `write_ref` /
//! `write_symbolic_ref` / `delete_ref`) and the tracking bookkeeping, but no
//! porcelain for branches. This module is the porcelain layer the app needs,
//! following the same "no shell out to git" rule as `ops`.
//!
//! The one deliberate departure from `git branch` / `git checkout` is in
//! `checkout_branch_clean`: git carries uncommitted changes across a checkout
//! when it can, and we do not -- grit has no three-way merge in the checkout
//! path, so carrying changes would silently drop half of them. Rather than
//! destroying a user's uncommitted notes, a dirty work tree is refused with an
//! error the UI can explain.

use anyhow::{anyhow, bail, Context, Result};
use grit_lib::branch_tracking::upstream_tracking_full_ref;
use grit_lib::check_ref_format::{check_refname_format, RefNameOptions};
use grit_lib::merge_base::count_symmetric_ahead_behind;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

use crate::ops;
use crate::types::BranchInfo;

/// Full ref for a short branch name.
fn branch_ref(name: &str) -> String {
    format!("refs/heads/{name}")
}

/// Reject names git would reject, before anything is written.
///
/// `check_refname_format` covers the D/F conflicts, trailing dots, control
/// characters and `..` segments; the two extra checks cover what is dangerous
/// for this layer specifically: a leading dash would be parsed as an option by
/// any future shell-out, and a name that already starts with `refs/` would
/// nest a ref inside a ref.
fn validate_branch_name(name: &str) -> Result<String> {
    if name.starts_with('-') {
        bail!("invalid branch name {name:?}: may not start with a dash");
    }
    if name.starts_with("refs/") {
        bail!("invalid branch name {name:?}: pass the short name, not the full ref");
    }
    let opts = RefNameOptions {
        allow_onelevel: true,
        refspec_pattern: false,
        normalize: true,
    };
    check_refname_format(name, &opts).map_err(|err| anyhow!("invalid branch name {name:?}: {err:?}"))?;
    Ok(name.to_owned())
}

/// Every local branch, sorted by name, with its upstream relation.
pub fn list_branches(repo: &Repository) -> Result<Vec<BranchInfo>> {
    let head_target = grit_lib::refs::read_symbolic_ref(&repo.git_dir, "HEAD")?;
    let mut out = Vec::new();
    for (refname, oid) in grit_lib::refs::list_refs(&repo.git_dir, "refs/heads/")? {
        let short = refname.strip_prefix("refs/heads/").unwrap_or(&refname).to_string();
        let mut info = BranchInfo {
            name: short.clone(),
            sha: oid.to_string(),
            is_head: head_target.as_deref() == Some(refname.as_str()),
            upstream: None,
            ahead: 0,
            behind: 0,
            upstream_gone: false,
        };
        if let Some(upstream_ref) = upstream_tracking_full_ref(repo, &short) {
            info.upstream = Some(upstream_ref.clone());
            // An upstream that was never fetched (or whose remote was deleted)
            // is a normal state, not an error: report it as "gone" with zero
            // counts instead of implying the branch is 0/0 with a live remote.
            match grit_lib::refs::resolve_ref(&repo.git_dir, &upstream_ref) {
                Ok(upstream_oid) => {
                    if let Ok((ahead, behind)) = count_symmetric_ahead_behind(repo, oid, upstream_oid) {
                        info.ahead = ahead as u32;
                        info.behind = behind as u32;
                    }
                }
                Err(_) => info.upstream_gone = true,
            }
        }
        out.push(info);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Create `name` at `from_sha` (HEAD when `None`) and return its sha.
///
/// Refuses to overwrite an existing branch: overwriting is what `git branch -f`
/// does, and it is never what a button click should mean.
pub fn create_branch(repo: &Repository, name: &str, from_sha: Option<&str>) -> Result<String> {
    let name = validate_branch_name(name)?;
    let refname = branch_ref(&name);
    if grit_lib::refs::resolve_ref(&repo.git_dir, &refname).is_ok() {
        bail!("branch {name:?} already exists");
    }
    let sha = match from_sha {
        Some(sha) => sha.to_owned(),
        None => ops::head_sha(repo).ok_or_else(|| anyhow!("HEAD does not point at a commit"))?,
    };
    let oid: ObjectId = sha.parse().with_context(|| format!("not an object id: {sha:?}"))?;
    // Confirm the object exists, so a typo cannot create a branch pointing at
    // nothing -- which would break every later read of the repository.
    repo.odb.read(&oid)?;
    grit_lib::refs::write_ref(&repo.git_dir, &refname, &oid)?;
    Ok(oid.to_string())
}

/// Point HEAD at an existing branch and update the work tree.
///
/// Refuses rather than overwriting local work; see the module comment.
pub fn checkout_branch_clean(repo: &Repository, name: &str) -> Result<String> {
    let name = validate_branch_name(name)?;
    let refname = branch_ref(&name);
    let target = ops::resolve_ref(repo, &refname).with_context(|| format!("branch {name:?} does not exist"))?;

    // Two distinct ways a checkout can destroy uncommitted work, and both have
    // to be ruled out before HEAD moves.
    let status = ops::status(repo)?;
    let dirty_tracked: Vec<&str> = status
        .files
        .iter()
        .filter(|f| f.staged || f.unstaged)
        .map(|f| f.path.as_str())
        .collect();
    if !dirty_tracked.is_empty() {
        bail!(
            "{} file(s) have uncommitted changes; commit or discard them before switching: {}",
            dirty_tracked.len(),
            dirty_tracked.join(", ")
        );
    }

    // The work-tree update overwrites whatever is at each target path, and an
    // untracked file counts as "whatever is there". git refuses that checkout;
    // so must we, or the file is simply gone.
    let untracked: Vec<&str> = status
        .files
        .iter()
        .filter(|f| !f.staged && !f.unstaged && f.status == "untracked")
        .map(|f| f.path.as_str())
        .collect();
    if !untracked.is_empty() {
        let target_paths = ops::list_files(repo, &target)?;
        let collisions: Vec<&str> = untracked
            .iter()
            .copied()
            .filter(|path| target_paths.iter().any(|p| p == path))
            .collect();
        if !collisions.is_empty() {
            bail!("switching would overwrite untracked file(s): {}", collisions.join(", "));
        }
    }

    ops::checkout_branch(repo, &name, &target)
}

/// Delete a local branch.
///
/// Refuses to delete the checked-out branch: git refuses too, and the failure
/// mode (HEAD pointing at a deleted ref) is a repository the user then has to
/// repair by hand. Without `force` the branch must be merged into HEAD.
pub fn delete_branch(repo: &Repository, name: &str, force: bool) -> Result<()> {
    let name = validate_branch_name(name)?;
    let refname = branch_ref(&name);
    let target = ops::resolve_ref(repo, &refname).with_context(|| format!("branch {name:?} does not exist"))?;

    if grit_lib::refs::read_symbolic_ref(&repo.git_dir, "HEAD")?.as_deref() == Some(refname.as_str()) {
        bail!("cannot delete {name:?}: it is the current branch");
    }

    if !force {
        if let Some(head) = ops::head_sha(repo) {
            let head_oid: ObjectId = head.parse().with_context(|| format!("not an object id: {head:?}"))?;
            let target_oid: ObjectId = target.parse().with_context(|| format!("not an object id: {target:?}"))?;
            if !grit_lib::merge_base::is_ancestor(repo, target_oid, head_oid)? {
                bail!("branch {name:?} is not fully merged; force-delete to discard its commits");
            }
        }
    }

    // `delete_ref` reports grit's own error type, so the conversion is
    // explicit rather than automatic.
    Ok(grit_lib::refs::delete_ref(&repo.git_dir, &refname)?)
}

