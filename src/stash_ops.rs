//! `git stash`: put the work in progress aside and get it back later.
//!
//! grit-lib ships the pieces -- `stash_worktree_change_paths` for what a stash
//! touches, `check_stash_apply_would_overwrite_local_changes` for the refusal
//! git makes before clobbering local edits -- but no command that assembles a
//! stash commit. That is what this file adds, using the same commit-writing
//! recipe `ops::commit_only` already follows.
//!
//! The on-disk layout is git's, so `git stash list` reads our stashes and we
//! read git's:
//!
//! - `refs/stash` points at the newest stash; the stack is the reflog.
//! - parents are `[HEAD, index-state, untracked-state]` -- the third parent
//!   only when `-u` was asked for, which is why untracked files are not in the
//!   stash tree itself.
//!
//! Applying restores the work tree and leaves the index alone, so restored
//! changes show up unstaged, which is what `git stash apply` does without
//! `--index`. `--index` (restoring the staged state too) is deliberately not
//! implemented: half of it would look like it worked.

use std::collections::BTreeSet;
use std::fs;

use anyhow::{anyhow, bail, Context, Result};
use grit_lib::index::Index;
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::progress::NullProgress;
use grit_lib::reflog;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::write_tree::write_tree_from_index;

use crate::ops::{self, add_path, append_reflog, core_filemode, set_executable, app_reflog_identity};

/// Where the stash stack lives. The ref names the top; the reflog is the stack.
const STASH_REF: &str = "refs/stash";

/// One entry of the stash stack.
pub struct StashEntry {
    /// Position in the stack, 0 being the most recent -- what `stash@{N}` means.
    pub index: usize,
    pub sha: String,
    /// The reflog message, which is also the commit subject.
    pub message: String,
}

/// The stash stack, newest first.
pub fn list_stashes(repo: &Repository) -> Result<Vec<StashEntry>> {
    let entries = reflog::read_reflog_dwim(&repo.git_dir, STASH_REF)?;
    let mut out = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().rev().enumerate() {
        out.push(StashEntry {
            index,
            sha: entry.new_oid.to_string(),
            message: entry.message.clone(),
        });
    }
    Ok(out)
}

/// Resolve `stash@{N}` or a raw commit id to the stash commit.
fn resolve_stash(repo: &Repository, selector: &str) -> Result<ObjectId> {
    if let Some(rest) = selector.strip_prefix("stash@{") {
        let body = rest
            .strip_suffix('}')
            .context("malformed stash selector; expected stash@{0}")?;
        let index: usize = body.parse().context("stash@{N} needs a number")?;
        let stashes = list_stashes(repo)?;
        let entry = stashes
            .get(index)
            .ok_or_else(|| anyhow!("there is no stash@{{{index}}}; the stack holds {}", stashes.len()))?;
        return entry
            .sha
            .parse()
            .with_context(|| format!("stash@{{{index}}} points at an unreadable commit"));
    }
    let oid: ObjectId = selector
        .parse()
        .with_context(|| format!("{selector} is not a stash@{{N}} selector or a commit id"))?;
    if !repo.odb.exists(&oid) {
        bail!("no stash with commit {selector}");
    }
    Ok(oid)
}

/// One path out of a tree, as `(mode, oid)`.
fn tree_entry(
    repo: &Repository,
    tree: ObjectId,
    path: &str,
) -> Result<Option<(u32, ObjectId)>> {
    for (rel, mode, oid) in ops::flatten_tree(&repo.odb, tree)? {
        if rel == path {
            return Ok(Some((mode, oid)));
        }
    }
    Ok(None)
}

/// Every path in a tree.
fn tree_paths(repo: &Repository, tree: ObjectId) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for (rel, _, _) in ops::flatten_tree(&repo.odb, tree)? {
        paths.push(rel);
    }
    Ok(paths)
}

fn commit_with_tree(
    repo: &Repository,
    tree: ObjectId,
    parents: Vec<ObjectId>,
    message: &str,
) -> Result<ObjectId> {
    let ident = ops::format_ident("NeuraScribe", "app@neurascribe.local");
    let commit = CommitData {
        tree,
        parents,
        author: ident.clone(),
        committer: ident,
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: format!("{}\n", message.trim_end()),
        raw_message: None,
    };
    Ok(repo.odb.write(ObjectKind::Commit, &serialize_commit(&commit))?)
}

/// Seven characters, the length git abbreviates to in a stash subject for a
/// repository this size.
fn abbrev(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The subject line of a commit, or an empty string if it cannot be read.
fn subject_of(repo: &Repository, sha: &str) -> String {
    let Ok(oid) = sha.parse::<ObjectId>() else {
        return String::new();
    };
    let Ok(obj) = repo.odb.read(&oid) else {
        return String::new();
    };
    let Ok(commit) = parse_commit(&obj.data) else {
        return String::new();
    };
    commit
        .message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Put the work in progress aside.
///
/// `message` replaces the generated subject; git writes `On <branch>: <message>`
/// in that case and `WIP on <branch>: <abbrev> <subject>` otherwise.
///
/// `include_untracked` is `-u`: untracked files go into a third parent rather
/// than the stash tree, and are deleted from the work tree afterwards.
pub fn push(
    repo: &Repository,
    message: Option<&str>,
    include_untracked: bool,
) -> Result<String> {
    let head = ops::head_sha(repo).context("nothing to stash: this repository has no commits yet")?;
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("bare repository has no work tree")?;
    let filemode = core_filemode(repo);

    let opts = grit_lib::porcelain::status::StatusOptions {
        untracked: grit_lib::porcelain::status::UntrackedMode::All,
        ignored: grit_lib::porcelain::status::IgnoredMode::No,
        renames: None,
        pathspecs: Vec::new(),
        ahead_behind: false,
    };
    let model = grit_lib::porcelain::status::status(repo, &opts, &mut NullProgress)?;

    // Tracked paths whose work tree differs from what is staged. The stash tree
    // starts from the index -- staged work is part of it -- and these are laid
    // on top.
    let mut tracked: BTreeSet<String> = BTreeSet::new();
    for entry in &model.unstaged {
        tracked.insert(entry.path().to_string());
    }
    let untracked: Vec<String> = if include_untracked {
        model.untracked.clone()
    } else {
        Vec::new()
    };

    if tracked.is_empty() && untracked.is_empty() {
        bail!("nothing to stash: the working tree is clean");
    }

    // Parent 1: HEAD as it is now.
    let head_oid: ObjectId = head.parse().context("parse HEAD sha")?;

    // Parent 2: what is staged.
    let index_tree = write_tree_from_index(&repo.odb, &repo.load_index()?, "")?;
    let index_commit = commit_with_tree(repo, index_tree, vec![head_oid], "index on stash")?;

    // The stash tree itself: the work tree as it looks right now.
    let mut working = repo.load_index()?;
    for rel in &tracked {
        let full = work_tree.join(rel);
        if full.is_file() {
            add_path(repo, &mut working, &full, filemode)?;
        } else {
            // Deleted in the work tree: a stash of a deletion is still a
            // deletion, so the entry goes rather than keeps the staged copy.
            working.entries.retain(|e| e.path != rel.as_bytes());
        }
    }
    let stash_tree = write_tree_from_index(&repo.odb, &working, "")?;

    let mut parents = vec![head_oid, index_commit];
    if !untracked.is_empty() {
        let mut untracked_index = Index::new();
        for rel in &untracked {
            add_path(repo, &mut untracked_index, &work_tree.join(rel), filemode)?;
        }
        let untracked_tree = write_tree_from_index(&repo.odb, &untracked_index, "")?;
        // Parentless: it is a bag of files, not a point in history.
        parents.push(commit_with_tree(repo, untracked_tree, Vec::new(), "untracked on stash")?);
    }

    let branch = ops::current_branch(repo).unwrap_or_else(|_| "HEAD".to_string());
    let subject = match message {
        Some(m) => format!("On {branch}: {}", m.trim_end()),
        None => format!("WIP on {branch}: {} {}", abbrev(&head), subject_of(repo, &head)),
    };
    let oid = commit_with_tree(repo, stash_tree, parents, &subject)?;

    // Everything above is object-database only: nothing the user can see moves
    // until the stash commit exists.
    let previous = refs::resolve_ref(&repo.git_dir, STASH_REF).ok();
    refs::write_ref(&repo.git_dir, STASH_REF, &oid)?;
    append_reflog(repo, STASH_REF, &previous.unwrap_or_else(ObjectId::zero), &oid, &app_reflog_identity(), &subject);

    // Back to HEAD in both the work tree and the index, which is what makes
    // the work "stashed" rather than merely recorded.
    ops::reset_hard_to(repo, &head)?;
    for rel in &untracked {
        let full = work_tree.join(rel);
        let _ = fs::remove_file(&full);
    }
    Ok(oid.to_string())
}

/// Restore a stash into the work tree, leaving it in the stack.
pub fn apply(repo: &Repository, selector: &str) -> Result<String> {
    let oid = resolve_stash(repo, selector)?;
    let obj = repo.odb.read(&oid)?;
    let stash = parse_commit(&obj.data)?;
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("bare repository has no work tree")?;

    // git refuses before touching anything, and names the files. Doing this
    // after the first write would leave a half-applied stash behind.
    grit_lib::porcelain::stash::check_stash_apply_would_overwrite_local_changes(
        repo, work_tree, &stash,
    )?;

    for rel in grit_lib::porcelain::stash::stash_worktree_change_paths(repo, &stash)? {
        let full = work_tree.join(&rel);
        match tree_entry(repo, stash.tree, &rel)? {
            Some((mode, blob)) => {
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                let data = repo.odb.read(&blob)?.data;
                fs::write(&full, &data)?;
                if mode == 0o100755 {
                    set_executable(&full)?;
                }
            }
            None => {
                let _ = fs::remove_file(&full);
            }
        }
    }

    // Untracked files live in the third parent and come back as untracked.
    if let Some(untracked_commit) = stash.parents.get(2) {
        let obj = repo.odb.read(untracked_commit)?;
        let tree = parse_commit(&obj.data)?.tree;
        for rel in tree_paths(repo, tree)? {
            let (_, blob) = tree_entry(repo, tree, &rel)?
                .ok_or_else(|| anyhow!("untracked stash entry {rel} vanished"))?;
            let full = work_tree.join(&rel);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&full, &repo.odb.read(&blob)?.data)?;
        }
    }

    Ok(oid.to_string())
}

/// Throw a stash away.
///
/// The entry leaves the reflog and `refs/stash` is repointed at whatever is
/// now on top; when that was the last one the ref goes away with it.
pub fn drop(repo: &Repository, selector: &str) -> Result<()> {
    let oid = resolve_stash(repo, selector)?;
    let entries = reflog::read_reflog_dwim(&repo.git_dir, STASH_REF)?;
    let position = entries
        .iter()
        .position(|e| e.new_oid == oid)
        .context("that stash is no longer in the stack")?;
    // `read_reflog` hands back file order (oldest first); `delete_reflog_entries`
    // numbers newest-first, the way `git reflog` displays them. Passing the
    // file position straight through deletes the wrong entry -- and because a
    // two-entry stack has no visible difference between "dropped the newest"
    // and "dropped the oldest" in the length alone, that bug reads like a
    // success until a third stash shows up.
    let newest_first = entries.len() - 1 - position;
    reflog::delete_reflog_entries(&repo.git_dir, STASH_REF, &[newest_first])?;

    let remaining = reflog::read_reflog_dwim(&repo.git_dir, STASH_REF)?;
    match remaining.last() {
        Some(top) => refs::write_ref(&repo.git_dir, STASH_REF, &top.new_oid)?,
        None => {
            let _ = refs::delete_ref(&repo.git_dir, STASH_REF);
        }
    }
    Ok(())
}

/// `apply` then `drop`: restore the work and take the stash out of the stack.
///
/// The drop only runs once the apply has succeeded -- a stash is the only copy
/// of that work, and dropping it on failure would destroy it.
pub fn pop(repo: &Repository, selector: &str) -> Result<String> {
    let sha = apply(repo, selector)?;
    drop(repo, selector)?;
    Ok(sha)
}

/// Paths a stash would restore, for a preview before the user commits to it.
pub fn preview_paths(repo: &Repository, selector: &str) -> Result<Vec<String>> {
    let oid = resolve_stash(repo, selector)?;
    let obj = repo.odb.read(&oid)?;
    let stash = parse_commit(&obj.data)?;
    let mut paths: Vec<String> = grit_lib::porcelain::stash::stash_worktree_change_paths(repo, &stash)?
        .into_iter()
        .collect();
    if let Some(untracked_commit) = stash.parents.get(2) {
        let obj = repo.odb.read(untracked_commit)?;
        let tree = parse_commit(&obj.data)?.tree;
        paths.extend(tree_paths(repo, tree)?);
    }
    paths.sort();
    Ok(paths)
}
